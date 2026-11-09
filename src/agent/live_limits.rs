//! Limits every agent of the daemon shares and a saved config change moves
//! while they run.

use aither_agent::DiskThresholds;

use crate::config::MayConfig;
use crate::tasks_md::schema::LineLimit;

/// The handles agents are built with whose values follow the config.
///
/// A clone shares the handles, so the daemon keeps one, passes it to every
/// agent build and [`apply`](Self::apply)s each config change to it.
#[derive(Debug, Clone)]
pub struct LiveLimits {
    disk: DiskThresholds,
    tasks_lines: LineLimit,
}

impl LiveLimits {
    /// Handles holding the values of `config`.
    #[must_use]
    pub fn of(config: &MayConfig) -> Self {
        let limits = Self {
            disk: DiskThresholds::default(),
            tasks_lines: LineLimit::default(),
        };
        limits.apply(config);
        limits
    }

    /// Moves every handle to the values of `config`.
    pub fn apply(&self, config: &MayConfig) {
        let disk = config.disk;
        self.disk.set(
            disk.yellow_free_bytes,
            disk.red_free_bytes,
            disk.final_free_bytes,
        );
        self.tasks_lines.set(config.tasks.line_limit);
    }

    /// The lines an agent's `tasks.md` may hold.
    #[must_use]
    pub fn tasks_lines(&self) -> LineLimit {
        self.tasks_lines.clone()
    }

    /// The thresholds the disk watch of every agent judges free space by.
    #[must_use]
    pub fn disk(&self) -> DiskThresholds {
        self.disk.clone()
    }
}

#[cfg(test)]
mod tests {
    use aither_agent::DiskPressure;

    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn the_default_config_keeps_aithers_thresholds() {
        let limits = LiveLimits::of(&MayConfig::default());
        let aither = DiskThresholds::default();
        for free in [
            0,
            3 * GIB - 1,
            3 * GIB,
            5 * GIB - 1,
            5 * GIB,
            10 * GIB - 1,
            10 * GIB,
        ] {
            assert_eq!(limits.disk().level(free), aither.level(free), "{free} free");
        }
    }

    #[test]
    fn a_config_change_moves_the_handle_agents_were_built_with() {
        let mut config = MayConfig::default();
        let limits = LiveLimits::of(&config);
        let built_with = limits.disk();
        assert_eq!(built_with.level(8 * GIB), DiskPressure::Yellow);

        config.disk.yellow_free_bytes = 6 * GIB;
        config.disk.red_free_bytes = 2 * GIB;
        config.disk.final_free_bytes = GIB;
        limits.apply(&config);

        assert_eq!(built_with.level(8 * GIB), DiskPressure::Normal);
        assert_eq!(built_with.level(4 * GIB), DiskPressure::Yellow);
        assert_eq!(built_with.level(GIB + 1), DiskPressure::Red);
        assert_eq!(built_with.level(GIB - 1), DiskPressure::Final);
    }

    #[test]
    fn a_config_change_moves_the_tasks_line_limit_agents_were_built_with() {
        let mut config = MayConfig::default();
        let limits = LiveLimits::of(&config);
        let built_with = limits.tasks_lines();
        assert_eq!(built_with.get(), 60);

        config.tasks.line_limit = 90;
        limits.apply(&config);
        assert_eq!(built_with.get(), 90);
    }

    #[test]
    fn the_disk_section_parses_from_toml() {
        let config: MayConfig =
            toml::from_str("[disk]\nyellow_free_bytes = 100\nred_free_bytes = 50\n")
                .expect("the settings parse");
        assert_eq!(config.disk.yellow_free_bytes, 100);
        assert_eq!(config.disk.red_free_bytes, 50);
        assert_eq!(
            config.disk.final_free_bytes,
            crate::config::DiskSettings::default().final_free_bytes,
            "an unset value keeps the default"
        );
    }
}
