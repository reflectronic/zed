use std::{
    hash::{Hash, Hasher},
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    App, AppContext, ClipboardItem, Context, Div, Entity, Hsla, InteractiveElement,
    ParentElement as _, Render, SerializedLocation, SerializedTaskTiming,
    SerializedThreadTaskTimings, SharedString, StatefulInteractiveElement, Styled, TaskTiming,
    ThreadTaskTimings, TitlebarOptions, UniformListScrollHandle, WeakEntity, WindowBounds,
    WindowOptions, div, prelude::FluentBuilder, px, relative, size, uniform_list,
};
use util::ResultExt;
use workspace::{
    Workspace,
    ui::{
        ActiveTheme, Button, ButtonCommon, ButtonStyle, Checkbox, Clickable, ContextMenu, Divider,
        DropdownMenu, ScrollAxes, ScrollableHandle as _, Scrollbars, ToggleState, Tooltip,
        WithScrollbar, h_flex, v_flex,
    },
};
use zed_actions::OpenPerformanceProfiler;

const NANOS_PER_MS: u128 = 1_000_000;
const VISIBLE_WINDOW_NANOS: u128 = 10 * 1_000_000_000;
const VISIBLE_WINDOW_DURATION: Duration = Duration::from_secs(10);

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

enum ProfileTimings {
    Foreground(Vec<TaskTiming>),
    AllThreads(Vec<ThreadTaskTimings>),
    // RemoteForeground(Vec<SerializedTaskTiming>),
    // RemoteAllThreads(Vec<SerializedThreadTaskTimings>),
}

impl ProfileTimings {
    fn is_empty(&self) -> bool {
        match self {
            ProfileTimings::Foreground(timings) => timings.is_empty(),
            ProfileTimings::AllThreads(threads) => threads.iter().all(|t| t.timings.is_empty()),
        }
    }
}

pub fn init(startup_time: Instant, cx: &mut App) {
    cx.observe_new(move |workspace: &mut workspace::Workspace, _, cx| {
        let workspace_handle = cx.entity().downgrade();
        workspace.register_action(move |_workspace, _: &OpenPerformanceProfiler, window, cx| {
            open_performance_profiler(startup_time, workspace_handle.clone(), window, cx);
        });
    })
    .detach();
}

fn open_performance_profiler(
    startup_time: Instant,
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

    let default_bounds = size(px(1280.), px(720.));

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
    startup_time: Instant,
    source: ProfileSource,
    timings: ProfileTimings,
    paused: bool,
    display_timings: Rc<Vec<SerializedTaskTiming>>,
    include_self_timings: ToggleState,
    autoscroll: bool,
    scroll_handle: UniformListScrollHandle,
    workspace: Option<WeakEntity<Workspace>>,
}

impl ProfilerWindow {
    pub fn new(
        startup_time: Instant,
        workspace_handle: Option<WeakEntity<Workspace>>,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|_cx| ProfilerWindow {
            startup_time,
            source: ProfileSource::Foreground,
            timings: ProfileTimings::Foreground(Vec::new()),
            paused: false,
            display_timings: Rc::new(Vec::new()),
            include_self_timings: ToggleState::Unselected,
            autoscroll: true,
            scroll_handle: UniformListScrollHandle::default(),
            workspace: workspace_handle,
        })
    }

    fn poll_timings(&mut self, cx: &App) {
        let dispatcher = cx.foreground_executor().dispatcher();
        let window_cutoff = Instant::now().checked_sub(VISIBLE_WINDOW_DURATION);
        let include_self = self.include_self_timings.selected();
        let anchor = self.startup_time;

        match self.source {
            ProfileSource::Foreground => {
                let data = dispatcher.get_current_thread_timings();
                let visible = visible_tail(&data, window_cutoff);
                let display = convert_and_filter(anchor, visible, include_self);
                self.display_timings = Rc::new(display);
                self.timings = ProfileTimings::Foreground(data);
            }
            ProfileSource::AllThreads => {
                let thread_data = dispatcher.get_all_timings();
                let visible_threads: Vec<Vec<SerializedTaskTiming>> = thread_data
                    .iter()
                    .map(|thread| {
                        let visible = visible_tail(&thread.timings, window_cutoff);
                        convert_and_filter(anchor, visible, include_self)
                    })
                    .collect();
                self.display_timings = Rc::new(kway_merge(visible_threads));
                self.timings = ProfileTimings::AllThreads(thread_data);
            }
        }
    }

    fn set_source(&mut self, source: ProfileSource) {
        if self.source == source {
            return;
        }
        self.source = source;
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
                            this.set_source(source);
                            cx.notify();
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
        let ui_font = theme::setup_ui_font(window, cx);
        if !self.paused {
            self.poll_timings(cx);
            window.request_animation_frame();
        }

        let scroll_offset = self.scroll_handle.offset();
        let max_offset = self.scroll_handle.max_offset();
        self.autoscroll = -scroll_offset.y >= (max_offset.height - px(24.));
        if self.autoscroll {
            self.scroll_handle.scroll_to_bottom();
        }

        let display_timings = self.display_timings.clone();

        v_flex()
            .id("profiler")
            .font(ui_font)
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
                                    if self.paused { "Resume" } else { "Pause" },
                                )
                                .style(ButtonStyle::Filled)
                                .on_click(cx.listener(
                                    |this, _, _window, cx| {
                                        this.paused = !this.paused;
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

                                        let anchor = this.startup_time;
                                        let serialized = match &this.timings {
                                            ProfileTimings::Foreground(data) => {
                                                let converted =
                                                    SerializedTaskTiming::convert(anchor, data);
                                                serde_json::to_string(&converted)
                                            }
                                            ProfileTimings::AllThreads(threads) => {
                                                let converted: Vec<SerializedThreadTaskTimings> =
                                                    threads
                                                        .iter()
                                                        .map(|t| {
                                                            SerializedThreadTaskTimings::convert(
                                                                anchor,
                                                                t.clone(),
                                                            )
                                                        })
                                                        .collect();
                                                serde_json::to_string(&converted)
                                            }
                                        };

                                        let Some(serialized) = serialized.log_err() else {
                                            return;
                                        };

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

                                            smol::fs::write(path, &serialized).await.log_err();
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
            .when(!display_timings.is_empty(), |div| {
                let min_nanos = display_timings[0].start;
                let now_nanos = Instant::now().duration_since(self.startup_time).as_nanos();
                let last = &display_timings[display_timings.len() - 1];
                let max_nanos = (last.start + last.duration).max(now_nanos);

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
                            uniform_list("list", display_timings.len(), {
                                let timings = display_timings.clone();
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
                                                color: cx.theme().accents().color_for_index(
                                                    location_color_index(&timing.location),
                                                ),
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
                        .custom_scrollbars(
                            Scrollbars::always_visible(ScrollAxes::Vertical)
                                .tracked_scroll_handle(&self.scroll_handle),
                            window,
                            cx,
                        ),
                )
            })
    }
}

fn visible_tail<'a>(timings: &'a [TaskTiming], cutoff: Option<Instant>) -> &'a [TaskTiming] {
    let Some(cutoff) = cutoff else {
        return timings;
    };
    let start = timings.partition_point(|t| t.start < cutoff);
    &timings[start..]
}

fn convert_and_filter(
    anchor: Instant,
    timings: &[TaskTiming],
    include_self: bool,
) -> Vec<SerializedTaskTiming> {
    timings
        .iter()
        .map(|timing| {
            let start = timing.start.duration_since(anchor).as_nanos();
            let duration = timing
                .end
                .unwrap_or_else(Instant::now)
                .duration_since(timing.start)
                .as_nanos();
            SerializedTaskTiming {
                location: timing.location.into(),
                start,
                duration,
            }
        })
        .filter(|timing| timing.duration / NANOS_PER_MS >= 1)
        .filter(|timing| include_self || !timing.location.file.ends_with("miniprofiler_ui.rs"))
        .collect()
}

fn location_color_index(location: &SerializedLocation) -> u32 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    location.file.hash(&mut hasher);
    location.line.hash(&mut hasher);
    location.column.hash(&mut hasher);
    hasher.finish() as u32
}

/// Merge K sorted `Vec<SerializedTaskTiming>` into a single sorted vec.
/// Each input vec must already be sorted by `start`.
fn kway_merge(lists: Vec<Vec<SerializedTaskTiming>>) -> Vec<SerializedTaskTiming> {
    let total_len: usize = lists.iter().map(|l| l.len()).sum();
    let mut result = Vec::with_capacity(total_len);
    let mut cursors = vec![0usize; lists.len()];

    loop {
        let mut min_start = u128::MAX;
        let mut min_list = None;

        for (list_idx, list) in lists.iter().enumerate() {
            let cursor = cursors[list_idx];
            if let Some(timing) = list.get(cursor) {
                if timing.start < min_start {
                    min_start = timing.start;
                    min_list = Some(list_idx);
                }
            }
        }

        match min_list {
            Some(idx) => {
                result.push(lists[idx][cursors[idx]].clone());
                cursors[idx] += 1;
            }
            None => break,
        }
    }

    result
}
