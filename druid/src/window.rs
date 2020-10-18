// Copyright 2019 The Druid Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Management of multiple windows.

use std::collections::HashMap;
use std::mem;

// Automatically defaults to std::time::Instant on non Wasm platforms
use instant::Instant;

use crate::kurbo::Rect;
use crate::piet::{Piet, RenderContext};
use crate::shell::{Counter, Cursor, Region, WindowHandle};

use crate::app::PendingWindow;
use crate::contexts::ContextState;
use crate::core::{CommandQueue, FocusChange, WidgetState};
use crate::modal::{Modal, ModalDesc};
use crate::util::ExtendDrain;
use crate::widget::LabelText;
use crate::win_handler::RUN_COMMANDS_TOKEN;
use crate::{
    BoxConstraints, Command, Data, Env, Event, EventCtx, ExtEventSink, Handled, InternalEvent,
    InternalLifeCycle, LayoutCtx, LifeCycle, LifeCycleCtx, MenuDesc, PaintCtx, Point, Size,
    TimerToken, UpdateCtx, Widget, WidgetId, WidgetPod,
};

/// A unique identifier for a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(u64);

/// Per-window state not owned by user code.
pub struct Window<T> {
    pub(crate) id: WindowId,
    pub(crate) root: WidgetPod<T, Box<dyn Widget<T>>>,
    pub(crate) title: LabelText<T>,
    size: Size,
    invalid: Region,
    pub(crate) menu: Option<MenuDesc<T>>,
    pub(crate) context_menu: Option<MenuDesc<T>>,
    pub(crate) last_anim: Option<Instant>,
    pub(crate) last_mouse_pos: Option<Point>,
    pub(crate) focus: Option<WidgetId>,
    pub(crate) handle: WindowHandle,
    pub(crate) timers: HashMap<TimerToken, WidgetId>,
    ext_handle: ExtEventSink,
    /// A stack of modal widgets. The last widget gets user interaction events, and can choose
    /// whether to propagate them.
    modals: Vec<Modal<T>>,
    // delegate?
}

impl<T> Window<T> {
    pub(crate) fn new(
        id: WindowId,
        handle: WindowHandle,
        pending: PendingWindow<T>,
        ext_handle: ExtEventSink,
    ) -> Window<T> {
        Window {
            id,
            root: WidgetPod::new(pending.root),
            size: Size::ZERO,
            invalid: Region::EMPTY,
            title: pending.title,
            menu: pending.menu,
            context_menu: None,
            last_anim: None,
            last_mouse_pos: None,
            focus: None,
            handle,
            timers: HashMap::new(),
            ext_handle,
            modals: Vec::new(),
        }
    }
}

impl<T: Data> Window<T> {
    /// `true` iff any child requested an animation frame since the last `AnimFrame` event.
    pub(crate) fn wants_animation_frame(&self) -> bool {
        self.root.state().request_anim
    }

    pub(crate) fn focus_chain(&self) -> &[WidgetId] {
        &self.root.state().focus_chain
    }

    /// Returns `true` if the provided widget may be in this window,
    /// but it may also be a false positive.
    /// However when this returns `false` the widget is definitely not in this window.
    pub(crate) fn may_contain_widget(&self, widget_id: WidgetId) -> bool {
        // The bloom filter we're checking can return false positives.
        widget_id == self.root.id() || self.root.state().children.may_contain(&widget_id)
    }

    pub(crate) fn set_menu(&mut self, mut menu: MenuDesc<T>, data: &T, env: &Env) {
        let platform_menu = menu.build_window_menu(data, env);
        self.handle.set_menu(platform_menu);
        self.menu = Some(menu);
    }

    pub(crate) fn show_context_menu(
        &mut self,
        mut menu: MenuDesc<T>,
        point: Point,
        data: &T,
        env: &Env,
    ) {
        let platform_menu = menu.build_popup_menu(data, env);
        self.handle.show_context_menu(platform_menu, point);
        self.context_menu = Some(menu);
    }

    /// On macos we need to update the global application menu to be the menu
    /// for the current window.
    #[cfg(target_os = "macos")]
    pub(crate) fn macos_update_app_menu(&mut self, data: &T, env: &Env) {
        if let Some(menu) = self.menu.as_mut().map(|m| m.build_window_menu(data, env)) {
            self.handle.set_menu(menu);
        }
    }

    fn post_event_processing(
        &mut self,
        widget_state: &mut WidgetState,
        queue: &mut CommandQueue,
        data: &T,
        env: &Env,
        process_commands: bool,
    ) {
        // If children are changed during the handling of an event,
        // we need to send RouteWidgetAdded now, so that they are ready for update/layout.
        if widget_state.children_changed {
            self.lifecycle(
                queue,
                &LifeCycle::Internal(InternalLifeCycle::RouteWidgetAdded),
                data,
                env,
                false,
            );
        }
        // Add all the requested timers to the window's timers map.
        self.timers.extend_drain(&mut widget_state.timers);

        // If we need a new paint pass, make sure druid-shell knows it.
        if self.wants_animation_frame() {
            self.handle.request_anim_frame();
        }
        self.invalid.union_with(&widget_state.invalid);

        // If there are any commands and they should be processed
        if process_commands && !queue.is_empty() {
            // Ask the handler to call us back on idle
            // so we can process them in a new event/update pass.
            if let Some(mut handle) = self.handle.get_idle_handle() {
                handle.schedule_idle(RUN_COMMANDS_TOKEN);
            } else {
                log::error!("failed to get idle handle");
            }
        }
    }

    fn handle_modal_command(&mut self, state: &mut WidgetState, cmd: &Command) -> Handled {
        if cmd.is(ModalDesc::<T>::SHOW_MODAL) {
            let modal = cmd.get_unchecked(ModalDesc::<T>::SHOW_MODAL);
            // SHOW_MODAL is private to druid, and we already checked at command submission
            // that the type parameter is correct.
            let modal = modal.take().unwrap().downcast::<ModalDesc<T>>().unwrap();
            self.modals.push((*modal).into());
            state.children_changed = true;
            // TODO: We could be more conservative about invalidation. The problem is that we
            // haven't laid out the modal yet, so we don't know how big it is. (also below)
            self.invalid.add_rect(self.size.to_rect());
            Handled::Yes
        } else if cmd.is(ModalDesc::DISMISS_MODAL) {
            if self.modals.pop().is_some() {
                state.children_changed = true;
            } else {
                log::warn!("cannot dismiss modal; no modal shown");
            }
            self.invalid.add_rect(self.size.to_rect());
            Handled::Yes
        } else if cmd.is(ModalDesc::SHOW_MODAL_NO_DATA) {
            if let Some(modal) = cmd.get_unchecked(ModalDesc::SHOW_MODAL_NO_DATA).take() {
                self.modals.push(modal.lensed().into());
                state.children_changed = true;
            } else {
                log::error!("couldn't get modal payload");
            }
            self.invalid.add_rect(self.size.to_rect());
            Handled::Yes
        } else {
            Handled::No
        }
    }

    pub(crate) fn event(
        &mut self,
        queue: &mut CommandQueue,
        event: Event,
        data: &mut T,
        env: &Env,
    ) -> Handled {
        match &event {
            Event::WindowSize(size) => self.size = *size,
            Event::MouseDown(e) | Event::MouseUp(e) | Event::MouseMove(e) | Event::Wheel(e) => {
                self.last_mouse_pos = Some(e.pos)
            }
            Event::Internal(InternalEvent::MouseLeave) => self.last_mouse_pos = None,
            _ => (),
        }

        let mut cursor = match event {
            Event::MouseMove(..) => Some(Cursor::Arrow),
            _ => None,
        };

        let event = match event {
            Event::Timer(token) => {
                if let Some(widget_id) = self.timers.get(&token) {
                    Event::Internal(InternalEvent::RouteTimer(token, *widget_id))
                } else {
                    log::error!("No widget found for timer {:?}", token);
                    return Handled::No;
                }
            }
            other => other,
        };

        if let Event::WindowConnected = event {
            self.lifecycle(
                queue,
                &LifeCycle::Internal(InternalLifeCycle::RouteWidgetAdded),
                data,
                env,
                false,
            );
        }

        let mut widget_state = WidgetState::new(self.root.id(), Some(self.size));
        let modal_cmd = if let Event::Command(c) = &event {
            self.handle_modal_command(&mut widget_state, c)
        } else {
            Handled::No
        };

        let is_handled = if modal_cmd == Handled::Yes {
            Handled::Yes
        } else {
            let mut state =
                ContextState::new::<T>(queue, &self.ext_handle, &self.handle, self.id, self.focus);
            let mut ctx = EventCtx {
                cursor: &mut cursor,
                state: &mut state,
                widget_state: &mut widget_state,
                is_handled: false,
                is_root: true,
            };

            for modal in self.modals.iter_mut().rev() {
                modal.widget.event(&mut ctx, &event, data, env);
                if is_user_input(&event) && !modal.pass_through_events {
                    ctx.set_handled();
                }
            }
            self.root.event(&mut ctx, &event, data, env);
            Handled::from(ctx.is_handled)
        };

        // Clean up the timer token and do it immediately after the event handling
        // because the token may be reused and re-added in a lifecycle pass below.
        if let Event::Internal(InternalEvent::RouteTimer(token, _)) = event {
            self.timers.remove(&token);
        }

        if let Some(focus_req) = widget_state.request_focus.take() {
            let old = self.focus;
            let new = self.widget_for_focus_request(focus_req);
            // Only send RouteFocusChanged in case there's actual change
            if old != new {
                let event = LifeCycle::Internal(InternalLifeCycle::RouteFocusChanged { old, new });
                self.lifecycle(queue, &event, data, env, false);
                self.focus = new;
            }
        }

        if let Some(cursor) = cursor {
            self.handle.set_cursor(&cursor);
        }

        self.post_event_processing(&mut widget_state, queue, data, env, false);

        is_handled
    }

    pub(crate) fn lifecycle(
        &mut self,
        queue: &mut CommandQueue,
        event: &LifeCycle,
        data: &T,
        env: &Env,
        process_commands: bool,
    ) {
        let mut widget_state = WidgetState::new(self.root.id(), Some(self.size));
        let mut state =
            ContextState::new::<T>(queue, &self.ext_handle, &self.handle, self.id, self.focus);
        let mut ctx = LifeCycleCtx {
            state: &mut state,
            widget_state: &mut widget_state,
        };
        for modal in self.modals.iter_mut().rev() {
            modal.widget.lifecycle(&mut ctx, event, data, env);
        }
        self.root.lifecycle(&mut ctx, event, data, env);
        self.post_event_processing(&mut widget_state, queue, data, env, process_commands);
    }

    pub(crate) fn update(&mut self, queue: &mut CommandQueue, data: &T, env: &Env) {
        self.update_title(data, env);

        let mut widget_state = WidgetState::new(self.root.id(), Some(self.size));
        let mut state =
            ContextState::new::<T>(queue, &self.ext_handle, &self.handle, self.id, self.focus);
        let mut update_ctx = UpdateCtx {
            widget_state: &mut widget_state,
            state: &mut state,
            prev_env: None,
            env,
        };

        for modal in self.modals.iter_mut().rev() {
            modal.widget.update(&mut update_ctx, data, env);
        }
        self.root.update(&mut update_ctx, data, env);
        self.post_event_processing(&mut widget_state, queue, data, env, false);
    }

    pub(crate) fn invalidate_and_finalize(&mut self) {
        if self.needs_layout() {
            self.handle.invalidate();
        } else {
            for rect in self.invalid.rects() {
                self.handle.invalidate_rect(*rect);
            }
            for modal in &self.modals {
                for rect in modal.widget.state().invalid.rects() {
                    let origin = modal.widget.state().paint_rect().origin().to_vec2();
                    self.handle.invalidate_rect(*rect + origin);
                }
            }

            // FIXME: We should unify the handling of invalid rects somehow. This exists because we
            // need to mark an invalid region when a modal *disappears*
            for rect in self.invalid.rects() {
                self.handle.invalidate_rect(*rect);
            }
            self.invalid.clear();
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn invalid(&self) -> &Region {
        &self.invalid
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn invalid_mut(&mut self) -> &mut Region {
        &mut self.invalid
    }

    /// Get ready for painting, by doing layout and sending an `AnimFrame` event.
    pub(crate) fn prepare_paint(&mut self, queue: &mut CommandQueue, data: &mut T, env: &Env) {
        let now = Instant::now();
        // TODO: this calculation uses wall-clock time of the paint call, which
        // potentially has jitter.
        //
        // See https://github.com/linebender/druid/issues/85 for discussion.
        let last = self.last_anim.take();
        let elapsed_ns = last.map(|t| now.duration_since(t).as_nanos()).unwrap_or(0) as u64;

        if self.needs_layout() {
            self.layout(queue, data, env);
        }

        // Here, `self.wants_animation_frame()` refers to the animation frame that is currently
        // being prepared for. (This is relying on the fact that `self.layout()` can't request
        // an animation frame.)
        if self.wants_animation_frame() {
            self.event(queue, Event::AnimFrame(elapsed_ns), data, env);
        }

        // Here, `self.wants_animation_frame()` is true if we want *another* animation frame after
        // the current one. (It got modified in the call to `self.event` above.)
        if self.wants_animation_frame() {
            self.last_anim = Some(now);
        }
    }

    fn needs_layout(&self) -> bool {
        self.root.state().needs_layout || self.modals.iter().any(|m| m.widget.state().needs_layout)
    }

    pub(crate) fn do_paint(
        &mut self,
        piet: &mut Piet,
        invalid: &Region,
        queue: &mut CommandQueue,
        data: &T,
        env: &Env,
    ) {
        piet.fill(
            invalid.bounding_box(),
            &env.get(crate::theme::WINDOW_BACKGROUND_COLOR),
        );
        self.paint(piet, invalid, queue, data, env);
    }

    fn layout(&mut self, queue: &mut CommandQueue, data: &T, env: &Env) {
        let mut widget_state = WidgetState::new(self.root.id(), Some(self.size));
        let mut state =
            ContextState::new::<T>(queue, &self.ext_handle, &self.handle, self.id, self.focus);
        let mut layout_ctx = LayoutCtx {
            state: &mut state,
            widget_state: &mut widget_state,
            mouse_pos: self.last_mouse_pos,
        };
        let bc = BoxConstraints::tight(self.size);
        let size = self.root.layout(&mut layout_ctx, &bc, data, env);
        self.root
            .set_layout_rect(&mut layout_ctx, data, env, size.to_rect());

        for modal in &mut self.modals {
            let bc = BoxConstraints::new(Size::ZERO, size);
            let modal_size = modal.widget.layout(&mut layout_ctx, &bc, data, env);
            let modal_origin = if let Some(pos) = modal.position {
                // TODO: translate the position to ensure that the modal fits in our bounds.
                pos
            } else {
                ((size.to_vec2() - modal_size.to_vec2()) / 2.0).to_point()
            };
            let modal_frame = Rect::from_origin_size(modal_origin, modal_size);
            modal
                .widget
                .set_layout_rect(&mut layout_ctx, data, env, modal_frame);
        }
        self.post_event_processing(&mut widget_state, queue, data, env, true);
    }

    /// only expose `layout` for testing; normally it is called as part of `do_paint`
    #[cfg(not(target_arch = "wasm32"))]
    #[cfg(test)]
    pub(crate) fn just_layout(&mut self, queue: &mut CommandQueue, data: &T, env: &Env) {
        self.layout(queue, data, env)
    }

    fn paint(
        &mut self,
        piet: &mut Piet,
        invalid: &Region,
        queue: &mut CommandQueue,
        data: &T,
        env: &Env,
    ) {
        let widget_state = WidgetState::new(self.root.id(), Some(self.size));
        let mut state =
            ContextState::new::<T>(queue, &self.ext_handle, &self.handle, self.id, self.focus);
        let mut ctx = PaintCtx {
            render_ctx: piet,
            state: &mut state,
            widget_state: &widget_state,
            z_ops: Vec::new(),
            region: invalid.clone(),
            depth: 0,
        };

        let root = &mut self.root;
        let modals = &mut self.modals;
        ctx.with_child_ctx(invalid.clone(), |ctx| {
            root.paint_raw(ctx, data, env);

            for modal in modals {
                modal.widget.paint(ctx, data, env);
            }
        });

        let mut z_ops = mem::take(&mut ctx.z_ops);
        z_ops.sort_by_key(|k| k.z_index);

        for z_op in z_ops.into_iter() {
            ctx.with_child_ctx(invalid.clone(), |ctx| {
                ctx.with_save(|ctx| {
                    ctx.render_ctx.transform(z_op.transform);
                    (z_op.paint_func)(ctx);
                });
            });
        }

        if self.wants_animation_frame() {
            self.handle.request_anim_frame();
        }
    }

    pub(crate) fn update_title(&mut self, data: &T, env: &Env) {
        if self.title.resolve(data, env) {
            self.handle.set_title(&self.title.display_text());
        }
    }

    pub(crate) fn get_menu_cmd(&self, cmd_id: u32) -> Option<Command> {
        self.context_menu
            .as_ref()
            .and_then(|m| m.command_for_id(cmd_id))
            .or_else(|| self.menu.as_ref().and_then(|m| m.command_for_id(cmd_id)))
    }

    fn widget_for_focus_request(&self, focus: FocusChange) -> Option<WidgetId> {
        match focus {
            FocusChange::Resign => None,
            FocusChange::Focus(id) => Some(id),
            FocusChange::Next => self.widget_from_focus_chain(true),
            FocusChange::Previous => self.widget_from_focus_chain(false),
        }
    }

    fn widget_from_focus_chain(&self, forward: bool) -> Option<WidgetId> {
        self.focus.and_then(|focus| {
            self.focus_chain()
                .iter()
                // Find where the focused widget is in the focus chain
                .position(|id| id == &focus)
                .map(|idx| {
                    // Return the id that's next to it in the focus chain
                    let len = self.focus_chain().len();
                    let new_idx = if forward {
                        (idx + 1) % len
                    } else {
                        (idx + len - 1) % len
                    };
                    self.focus_chain()[new_idx]
                })
                .or_else(|| {
                    // If the currently focused widget isn't in the focus chain,
                    // then we'll just return the first/last entry of the chain, if any.
                    if forward {
                        self.focus_chain().first().copied()
                    } else {
                        self.focus_chain().last().copied()
                    }
                })
        })
    }
}

fn is_user_input(event: &Event) -> bool {
    match event {
        Event::MouseUp(_)
        | Event::MouseDown(_)
        | Event::MouseMove(_)
        | Event::KeyUp(_)
        | Event::KeyDown(_)
        | Event::Paste(_)
        | Event::Wheel(_)
        | Event::Zoom(_) => true,
        _ => false,
    }
}

impl WindowId {
    /// Allocate a new, unique window id.
    pub fn next() -> WindowId {
        static WINDOW_COUNTER: Counter = Counter::new();
        WindowId(WINDOW_COUNTER.next())
    }
}
