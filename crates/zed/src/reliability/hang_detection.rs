use std::any::TypeId;
use std::panic::Location;
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use collections::HashMap;
use gpui::{AppContext, TasksIncluded, profiler};
use log::info;
use ui::App;

mod task_traces;

gpui::actions!(
    dev,
    [
        /// Causes a performance hang to test performance monitoring
        HangAction,
        /// Causes a performance hang to test performance monitoring
        HangBackground,
        /// Causes a performance hang to test performance monitoring
        HangForeground,
    ]
);

pub(crate) fn start(cx: &mut App) {
    let hang_time = if cfg!(debug_assertions) {
        if cfg!(windows) {
            // yes windows debug builds are horribly slow
            Duration::from_secs(30)
        } else {
            Duration::from_secs(5)
        }
    } else {
        Duration::from_millis(10)
    };

    if cfg!(debug_assertions) {
        log::warn!("debug build, only reporting hangs longer then {hang_time:?}");
    }

    start_hang_detection(cx, hang_time);

    cx.on_action(move |_: &HangAction, _| {
        log::warn!(
            "Hanging the foreground for {hang_time:?} by blocking in an action. \
            Zed will be unresponsive for that time. This should trigger a report in the log",
        );
        thread::sleep(hang_time + Duration::from_micros(1));
        log::warn!("Hang ended");
    });
    cx.on_action(move |_: &HangBackground, cx| {
        cx.background_spawn(async move {
            log::warn!(
                "Hanging one background executor for {hang_time:?}. \
                This should trigger a report in the log",
            );
            thread::sleep(hang_time + Duration::from_micros(1));
            log::warn!("Hang ended");
        })
        .detach();
    });
    cx.on_action(move |_: &HangForeground, cx| {
        cx.spawn(async move |_| {
            log::warn!(
                "Hanging the foreground executor for {hang_time:?} seconds to test \
                performance monitoring! Zed will be unresponsive for that time. \
                This should trigger a report in the log"
            );
            thread::sleep(hang_time + Duration::from_micros(1));
            log::warn!("Hang ended");
        })
        .detach();
    });
}

fn start_hang_detection(cx: &App, report_longer_then: Duration) {
    let foreground_thread = thread::current().id();
    let action_resolver = cx.__action_resolver();
    let background_executor = cx.background_executor().clone();

    // an OS thread to insulate detection and reporting from hangs on the fore
    // or background.
    thread::Builder::new()
        .name("HangDetection".to_string())
        .spawn(move || {
            let mut recent = RecentlyReported::new();
            loop {
                thread::sleep(Duration::from_secs(1));
                let task_stats = background_executor
                    .dispatcher()
                    .get_all_stats(TasksIncluded::CompletedAndRunning);

                let mut reported_task_hangs = false;
                reported_task_hangs |= report_hanging_foreground(
                    &mut recent,
                    &task_stats,
                    report_longer_then,
                    foreground_thread,
                );
                reported_task_hangs |= report_hanging_background(
                    &mut recent,
                    &task_stats,
                    report_longer_then,
                    foreground_thread,
                );
                report_hanging_actions(&mut recent, &action_resolver, report_longer_then);

                if reported_task_hangs
                    && let Some(path) =
                        task_traces::save_any(&background_executor, foreground_thread)
                {
                    log::info!("Task trace has been saved to: {}", path.display());
                }
            }
        })
        .expect("App can always spawn threads");
}

#[derive(Debug, Hash, Eq, PartialEq)]
enum PerfIssue {
    Foreground(&'static Location<'static>),
    Background(&'static Location<'static>),
    Action(TypeId),
}

struct RecentlyReported {
    forget_after: Duration,
    history: HashMap<PerfIssue, Instant>,
}

impl RecentlyReported {
    fn recently(&self, issue: PerfIssue) -> bool {
        self.history
            .get(&issue)
            .is_some_and(|reported_at| reported_at.elapsed() < self.forget_after)
    }
    fn update(&mut self, new: impl Iterator<Item = PerfIssue>) {
        let _ = self
            .history
            .extract_if(|_, reported_at| reported_at.elapsed() > self.forget_after)
            .count();

        let now = Instant::now();
        for issue in new {
            if !self.history.contains_key(&issue) {
                self.history.insert(issue, now);
            }
        }
    }
    fn new() -> Self {
        Self {
            forget_after: Duration::from_mins(5),
            history: HashMap::default(),
        }
    }
}

type ReportMade = bool;
fn report_hanging_foreground(
    reported: &mut RecentlyReported,
    task_stats: &[gpui::ThreadTaskStatistics],
    report_longer_then: Duration,
    foreground_thread: ThreadId,
) -> ReportMade {
    let foreground = task_stats
        .iter()
        .find(|t| t.thread_id == foreground_thread)
        .expect("main thread should be in all statistics");

    if foreground
        .stats
        .longest_poll_times
        .iter()
        .filter(|task| !reported.recently(PerfIssue::Foreground(task.location)))
        .any(|task| task.poll_duration() > report_longer_then)
    {
        reported.update(
            foreground
                .stats
                .longest_poll_times
                .iter()
                .map(|task| PerfIssue::Foreground(task.location)),
        );
        info!("New foreground hang detected:\n\t{}", foreground.stats);
        true
    } else {
        false
    }
}

fn report_hanging_background(
    reported: &mut RecentlyReported,
    task_stats: &[gpui::ThreadTaskStatistics],
    report_longer_then: Duration,
    foreground_thread: ThreadId,
) -> ReportMade {
    let background = task_stats
        .iter()
        .filter(|t| t.thread_id != foreground_thread);

    let mut report_made = false;
    for worker in background {
        if worker
            .stats
            .longest_poll_times
            .iter()
            .filter(|task| !reported.recently(PerfIssue::Background(task.location)))
            .any(|stat| stat.poll_duration() > report_longer_then)
        {
            reported.update(
                worker
                    .stats
                    .longest_poll_times
                    .iter()
                    .map(|task| PerfIssue::Background(task.location)),
            );
            info!(
                "Background hang detected on {}:\n{}",
                worker.thread_name.as_deref().unwrap_or_else(|| "Unknown"),
                worker.stats
            );
            report_made = true;
        }
    }
    report_made
}

fn report_hanging_actions(
    reported: &mut RecentlyReported,
    resolver: &gpui::ActionResolver,
    report_longer_then: Duration,
) {
    let stats = profiler::collect_action_stats();

    if stats
        .longest_runtimes()
        .filter(|action| !reported.recently(PerfIssue::Action(action.id)))
        .any(|action| action.runtime() > report_longer_then)
    {
        reported.update(
            stats
                .longest_runtimes()
                .map(|action| PerfIssue::Action(action.id)),
        );
        let stats = stats.resolve(resolver);
        info!("Action hang detected:\n\t{}", stats);
    }
}
