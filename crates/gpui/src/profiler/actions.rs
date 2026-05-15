use std::{
    any::TypeId,
    hint::cold_path,
    time::{Duration, Instant},
};

use crate::{ActionRegistry, action::Action};

#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct ActionStatistics {
    runtime_to_beat: Duration,
    pub longest_runtimes: heapless::Vec<ActionTiming, 5>,
    running: Option<(TypeId, Instant)>,
}

// TODO!(yara) not here but, we should have a running action to inspect during
// mega lag. The overhead from that is super worth it.

impl std::fmt::Display for ResolvedActionStatistics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Actions that blocked the longest")?;
        for ResolvedActionTiming {
            action_name,
            start,
            end,
        } in &self.0
        {
            let took = end.duration_since(*start);
            f.write_fmt(format_args!("\n {took:?}: {action_name}\n",))?;
        }
        Ok(())
    }
}

impl ActionStatistics {
    const fn new() -> Self {
        Self {
            runtime_to_beat: Duration::ZERO,
            longest_runtimes: heapless::Vec::new(),
            running: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.longest_runtimes.is_empty()
    }

    pub fn update_running_action(&mut self, action: TypeId, started: Instant) {
        self.running = Some((action, started));
    }

    pub fn save_action_timing(&mut self) {
        let now = Instant::now();
        let (action, started) = self
            .running
            .take()
            .expect("only called after `update_running_action`");

        let runtime = started.duration_since(now);
        if runtime >= self.runtime_to_beat {
            cold_path(); // most actions are not the worst, optimize for that
            if let Some(to_replace) = self
                .longest_runtimes
                .iter_mut()
                .min_by_key(|action| runtime >= action.runtime())
            {
                *to_replace = ActionTiming {
                    id: action,
                    start: started,
                    end: now,
                };
            } else {
                self.longest_runtimes
                    .push(ActionTiming {
                        id: action,
                        start: started,
                        end: now,
                    })
                    .expect("it must be empty or we would have found min_by_pos");
            };

            self.runtime_to_beat = self
                .longest_runtimes
                .iter()
                .map(|action| action.runtime())
                .min()
                .expect("never empty");
        }
    }

    pub fn resolve(self, resolver: &crate::ActionResolver) -> ResolvedActionStatistics {
        ResolvedActionStatistics(
            self.longest_runtimes
                .into_iter()
                .flat_map(|timing| timing.try_resolve(&resolver.0))
                .collect(),
        )
    }

    pub fn worst_time(&self) -> Duration {
        self.longest_runtimes
            .iter()
            .map(ActionTiming::runtime)
            .max()
            .unwrap_or(Duration::ZERO)
    }
}

/// Resolved variant of [`ActionTiming`] where the actions are resolved (use
/// names instead of type ids)
#[derive(Debug, Clone)]
pub struct ResolvedActionStatistics(pub Vec<ResolvedActionTiming>);
impl ResolvedActionStatistics {
    #[doc(hidden)]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    #[doc(hidden)]
    pub fn empty() -> Self {
        Self(Vec::new())
    }
}

#[doc(hidden)]
/// UNSTABLE only for use in the profiler and zed-reliability
#[derive(Copy, Clone, Debug)]
pub struct ActionTiming {
    pub id: TypeId,
    pub start: Instant,
    pub end: Instant,
}

impl ActionTiming {
    pub fn runtime(&self) -> Duration {
        self.end - self.start
    }
    fn try_resolve(self, actions: &ActionRegistry) -> Option<ResolvedActionTiming> {
        match actions.try_resolve_action(&self.id) {
            Some(action_name) => Some(ResolvedActionTiming {
                action_name,
                start: self.start,
                end: self.end,
            }),
            None => {
                cold_path();
                log::error!("Profiler could not resolve action name");
                None
            }
        }
    }
}

/// Resolved variant of [`ActionTiming`] with Type_Id replaced with the Action's
/// name instead.
#[derive(Debug, Clone)]
pub struct ResolvedActionTiming {
    pub action_name: &'static str,
    pub start: Instant,
    pub end: Instant,
}

impl ResolvedActionTiming {
    #[doc(hidden)]
    pub fn runtime(&self) -> Duration {
        self.end - self.start
    }
}

// The profiler is careful to never block when the lock is held, therefore a
// spinlock is optimal.
static ACTION_STATISTICS: spin::Mutex<ActionStatistics> =
    const { spin::Mutex::new(ActionStatistics::new()) };

#[doc(hidden)]
pub(crate) fn update_running_action(action: &(dyn Action + 'static)) {
    let now = Instant::now();
    let action = action.type_id();
    ACTION_STATISTICS.lock().update_running_action(action, now);
}

#[doc(hidden)]
pub(crate) fn save_action_timing() {
    ACTION_STATISTICS.lock().save_action_timing();
}

#[doc(hidden)]
pub fn collect_action_stats() -> ActionStatistics {
    ACTION_STATISTICS.lock().clone()
}
