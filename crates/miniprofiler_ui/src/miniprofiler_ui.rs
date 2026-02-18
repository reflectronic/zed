use std::{path::PathBuf, rc::Rc, time::Duration};

use gpui::{
    App, AppContext, ClipboardItem, Context, Div, Entity, Hsla, InteractiveElement,
    ParentElement as _, Render, SerializedLocation, SerializedTaskTiming,
    SerializedThreadTaskTimings, SharedString, StatefulInteractiveElement, Styled, Task,
    TitlebarOptions, UniformListScrollHandle, WeakEntity, WindowBounds, WindowOptions, div,
    prelude::FluentBuilder, px, relative, size, uniform_list,
};
use util::ResultExt;
use workspace::{
    Workspace,
    ui::{
        ActiveTheme, Button, ButtonCommon, ButtonStyle, Checkbox, Clickable, ContextMenu, Divider,
        DropdownMenu, ScrollableHandle as _, ToggleState, Tooltip, WithScrollbar, h_flex, v_flex,
    },
};
use zed_actions::OpenPerformanceProfiler;

const NANOS_PER_MS: u128 = 1_000_000;
const VISIBLE_WINDOW_NANOS: u128 = 10 * 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileSource {
    Foreground,
    AllThreads,
}

impl ProfileSource {
    fn label(&self) -> &'static str {
        match self {
            ProfileSource::Foreground => "Foreground",
            ProfileSource::AllThreads => "All threads",
        }
    }
}

#[derive(Debug, Clone)]
enum ProfileTimings {
    Foreground(Vec<SerializedTaskTiming>),
    AllThreads(Vec<SerializedThreadTaskTimings>),
}

impl ProfileTimings {
    fn flattened_timings(&self) -> Vec<SerializedTaskTiming> {
        match self {
            ProfileTimings::Foreground(timings) => timings.clone(),
            ProfileTimings::AllThreads(threads) => {
                let mut all_timings = Vec::new();
                for thread in threads {
                    all_timings.extend(thread.timings.iter().cloned());
                }
                all_timings.sort_by_key(|t| t.start);
                all_timings
            }
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            ProfileTimings::Foreground(timings) => timings.is_empty(),
            ProfileTimings::AllThreads(threads) => threads.iter().all(|t| t.timings.is_empty()),
        }
    }
}

pub fn init(startup_time: std::time::Instant, cx: &mut App) {
    cx.observe_new(move |workspace: &mut workspace::Workspace, _, cx| {
        let workspace_handle = cx.entity().downgrade();
        workspace.register_action(move |_workspace, _: &OpenPerformanceProfiler, window, cx| {
            open_performance_profiler(startup_time, workspace_handle.clone(), window, cx);
        });
    })
    .detach();
}

fn open_performance_profiler(
    startup_time: std::time::Instant,
    workspace_handle: WeakEntity<Workspace>,
    _window: &mut gpui::Window,
    cx: &mut App,
) {
    let existing_window = cx
        .windows()
        .into_iter()
        .find_map(|window| window.downcast::<ProfilerWindow>());

    if let Some(existing_window) = existing_window {
        existing_window
            .update(cx, |profiler_window, window, _cx| {
                profiler_window.workspace = Some(workspace_handle.clone());
                window.activate_window();
            })
            .log_err();
        return;
    }

    let default_bounds = size(px(1280.), px(720.)); // 16:9

    cx.open_window(
        WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("Profiler Window".into()),
                appears_transparent: false,
                traffic_light_position: None,
            }),
            focus: true,
            show: true,
            is_movable: true,
            kind: gpui::WindowKind::Normal,
            window_background: cx.theme().window_background_appearance(),
            window_decorations: None,
            window_min_size: Some(default_bounds),
            window_bounds: Some(WindowBounds::centered(default_bounds, cx)),
            ..Default::default()
        },
        |_window, cx| ProfilerWindow::new(startup_time, Some(workspace_handle), cx),
    )
    .log_err();
}

struct TimingBar {
    location: SerializedLocation,
    start_nanos: u128,
    duration_nanos: u128,
    color: Hsla,
}

pub struct ProfilerWindow {
    startup_time: std::time::Instant,
    source: ProfileSource,
    timings: ProfileTimings,
    include_self_timings: ToggleState,
    autoscroll: bool,
    scroll_handle: UniformListScrollHandle,
    workspace: Option<WeakEntity<Workspace>>,
    _refresh: Option<Task<()>>,
}

impl ProfilerWindow {
    pub fn new(
        startup_time: std::time::Instant,
        workspace_handle: Option<WeakEntity<Workspace>>,
        cx: &mut App,
    ) -> Entity<Self> {
        let source = ProfileSource::Foreground;
        cx.new(|cx| ProfilerWindow {
            startup_time,
            source,
            timings: ProfileTimings::Foreground(Vec::new()),
            include_self_timings: ToggleState::Unselected,
            autoscroll: true,
            scroll_handle: UniformListScrollHandle::default(),
            workspace: workspace_handle,
            _refresh: Some(Self::begin_listen(source, cx)),
        })
    }

    fn begin_listen(source: ProfileSource, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let timings = match source {
                    ProfileSource::Foreground => {
                        let data = cx
                            .foreground_executor()
                            .dispatcher()
                            .get_current_thread_timings();
                        this.update(cx, |this: &mut ProfilerWindow, _cx| {
                            ProfileTimings::Foreground(SerializedTaskTiming::convert(
                                this.startup_time,
                                &data,
                            ))
                        })
                        .ok()
                    }
                    ProfileSource::AllThreads => {
                        let thread_timings =
                            cx.foreground_executor().dispatcher().get_all_timings();
                        this.update(cx, |this: &mut ProfilerWindow, _cx| {
                            ProfileTimings::AllThreads(
                                thread_timings
                                    .into_iter()
                                    .map(|thread| {
                                        SerializedThreadTaskTimings::convert(
                                            this.startup_time,
                                            thread,
                                        )
                                    })
                                    .collect(),
                            )
                        })
                        .ok()
                    }
                };

                if let Some(timings) = timings {
                    this.update(cx, |this: &mut ProfilerWindow, cx| {
                        this.timings = timings;
                        cx.notify();
                    })
                    .ok();
                }

                cx.background_executor()
                    .timer(Duration::from_micros(1))
                    .await;
            }
        })
    }

    fn is_paused(&self) -> bool {
        self._refresh.is_none()
    }

    fn set_source(&mut self, source: ProfileSource, cx: &mut Context<Self>) {
        if self.source == source {
            return;
        }
        self.source = source;
        if !self.is_paused() {
            self._refresh = Some(Self::begin_listen(source, cx));
        }
        cx.notify();
    }

    fn render_source_dropdown(
        &self,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) -> DropdownMenu {
        let weak = cx.weak_entity();
        let current_source = self.source;

        let sources = [ProfileSource::Foreground, ProfileSource::AllThreads];

        DropdownMenu::new(
            "profile-source",
            current_source.label(),
            ContextMenu::build(window, cx, move |mut menu, window, cx| {
                for source in sources {
                    let weak = weak.clone();
                    menu = menu.entry(source.label(), None, move |_, cx| {
                        weak.update(cx, |this, cx| {
                            this.set_source(source, cx);
                        })
                        .log_err();
                    });
                }
                if let Some(index) = sources.iter().position(|s| *s == current_source) {
                    for _ in 0..=index {
                        menu.select_next(&Default::default(), window, cx);
                    }
                }
                menu
            }),
        )
    }

    fn render_timing(
        window_start_nanos: u128,
        window_duration_nanos: u128,
        item: TimingBar,
        cx: &App,
    ) -> Div {
        let time_ms = item.duration_nanos as f32 / NANOS_PER_MS as f32;

        let start_fraction = if item.start_nanos >= window_start_nanos {
            (item.start_nanos - window_start_nanos) as f32 / window_duration_nanos as f32
        } else {
            0.0
        };

        let end_nanos = item.start_nanos + item.duration_nanos;
        let end_fraction = if end_nanos >= window_start_nanos {
            (end_nanos - window_start_nanos) as f32 / window_duration_nanos as f32
        } else {
            0.0
        };

        let bar_width = (end_fraction - start_fraction).max(0.0);

        let file_str: &str = &item.location.file;
        let basename = file_str.rsplit_once("/").unwrap_or(("", file_str)).1;
        let basename = basename.rsplit_once("\\").unwrap_or(("", basename)).1;

        let label = SharedString::from(format!(
            "{}:{}:{}",
            basename, item.location.line, item.location.column
        ));

        h_flex()
            .gap_2()
            .w_full()
            .h(px(32.0))
            .child(
                div()
                    .id(label.clone())
                    .w(px(200.0))
                    .flex_shrink_0()
                    .overflow_hidden()
                    .child(div().text_ellipsis().child(label.clone()))
                    .tooltip(Tooltip::text(label.clone()))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(label.to_string()))
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .h(px(24.0))
                    .bg(cx.theme().colors().background)
                    .rounded_md()
                    .p(px(2.0))
                    .relative()
                    .child(
                        div()
                            .absolute()
                            .h_full()
                            .rounded_sm()
                            .bg(item.color)
                            .left(relative(start_fraction.max(0.0)))
                            .w(relative(bar_width)),
                    ),
            )
            .child(
                div()
                    .min_w(px(70.))
                    .flex_shrink_0()
                    .text_right()
                    .child(format!("{:.1} ms", time_ms)),
            )
    }
}

impl Render for ProfilerWindow {
    fn render(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        let scroll_offset = self.scroll_handle.offset();
        let max_offset = self.scroll_handle.max_offset();
        self.autoscroll = -scroll_offset.y >= (max_offset.height - px(24.));
        if self.autoscroll {
            self.scroll_handle.scroll_to_bottom();
        }

        v_flex()
            .id("profiler")
            .w_full()
            .h_full()
            .bg(cx.theme().colors().surface_background)
            .text_color(cx.theme().colors().text)
            .child(
                h_flex()
                    .py_2()
                    .px_4()
                    .w_full()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(self.render_source_dropdown(window, cx))
                            .child(
                                Button::new(
                                    "switch-mode",
                                    if self.is_paused() { "Resume" } else { "Pause" },
                                )
                                .style(ButtonStyle::Filled)
                                .on_click(cx.listener(
                                    |this, _, _window, cx| {
                                        if this.is_paused() {
                                            this._refresh =
                                                Some(Self::begin_listen(this.source, cx));
                                        } else {
                                            this._refresh = None;
                                        }
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                Button::new("export-data", "Save")
                                    .style(ButtonStyle::Filled)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        let Some(workspace) = this.workspace.as_ref() else {
                                            return;
                                        };

                                        if this.timings.is_empty() {
                                            return;
                                        }
                                        let timings = this.timings.clone();

                                        let active_path = workspace
                                            .read_with(cx, |workspace, cx| {
                                                workspace.most_recent_active_path(cx)
                                            })
                                            .log_err()
                                            .flatten()
                                            .and_then(|p| p.parent().map(|p| p.to_owned()))
                                            .unwrap_or_else(PathBuf::default);

                                        let path = cx.prompt_for_new_path(
                                            &active_path,
                                            Some("performance_profile.miniprof"),
                                        );

                                        cx.background_spawn(async move {
                                            let path = path.await;
                                            let path =
                                                path.log_err().and_then(|p| p.log_err()).flatten();

                                            let Some(path) = path else {
                                                return;
                                            };

                                            let serialized = match &timings {
                                                ProfileTimings::Foreground(t) => {
                                                    serde_json::to_string(t)
                                                }
                                                ProfileTimings::AllThreads(t) => {
                                                    serde_json::to_string(t)
                                                }
                                            };
                                            let Some(timings) = serialized.log_err() else {
                                                return;
                                            };

                                            smol::fs::write(path, &timings).await.log_err();
                                        })
                                        .detach();
                                    })),
                            ),
                    )
                    .child(
                        Checkbox::new("include-self", self.include_self_timings)
                            .label("Include profiler timings")
                            .on_click(cx.listener(|this, checked, _window, cx| {
                                this.include_self_timings = *checked;
                                cx.notify();
                            })),
                    ),
            )
            .when(!self.timings.is_empty(), |div| {
                let timings = self.timings.flattened_timings();

                if timings.is_empty() {
                    return div;
                }

                let min_nanos = timings[0].start;
                let last = &timings[timings.len() - 1];
                let max_nanos = last.start + last.duration;

                let timings = Rc::new(
                    timings
                        .iter()
                        .filter(|timing| timing.duration / NANOS_PER_MS >= 1)
                        .filter(|timing| {
                            if self.include_self_timings.selected() {
                                true
                            } else {
                                !timing.location.file.ends_with("miniprofiler_ui.rs")
                            }
                        })
                        .cloned()
                        .collect::<Vec<_>>(),
                );

                let window_start_nanos = max_nanos
                    .saturating_sub(VISIBLE_WINDOW_NANOS)
                    .max(min_nanos);
                let window_duration_nanos = max_nanos.saturating_sub(window_start_nanos).max(1);

                div.child(Divider::horizontal()).child(
                    v_flex()
                        .id("timings.bars")
                        .w_full()
                        .h_full()
                        .gap_2()
                        .child(
                            uniform_list("list", timings.len(), {
                                let timings = timings.clone();
                                move |visible_range, _, cx| {
                                    let mut items = vec![];
                                    for i in visible_range {
                                        let timing = &timings[i];
                                        items.push(Self::render_timing(
                                            window_start_nanos,
                                            window_duration_nanos,
                                            TimingBar {
                                                location: timing.location.clone(),
                                                start_nanos: timing.start,
                                                duration_nanos: timing.duration,
                                                color: cx
                                                    .theme()
                                                    .accents()
                                                    .color_for_index(i as u32),
                                            },
                                            cx,
                                        ));
                                    }
                                    items
                                }
                            })
                            .p_4()
                            .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                                this.autoscroll = false;
                                cx.notify();
                            }))
                            .track_scroll(&self.scroll_handle)
                            .size_full(),
                        )
                        .vertical_scrollbar_for(&self.scroll_handle, window, cx),
                )
            })
    }
}
