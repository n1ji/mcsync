//! Find the Minecraft (java) process running from an instance's game dir.

use std::path::Path;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

fn norm(p: &str) -> String {
    let s = p.replace('\\', "/").trim_end_matches('/').to_string();
    if cfg!(windows) || cfg!(target_os = "macos") {
        s.to_lowercase()
    } else {
        s
    }
}

fn refresh() -> System {
    System::new_with_specifics(
        RefreshKind::nothing().with_processes(
            ProcessRefreshKind::nothing()
                .with_cmd(UpdateKind::Always)
                .with_cwd(UpdateKind::Always)
                .with_exe(UpdateKind::OnlyIfNotSet),
        ),
    )
}

/// PIDs of java processes whose working dir is `game_dir` or whose command
/// line passes it (`--gameDir`, `-Djava.library.path=...` etc.).
pub fn find_game_processes(game_dir: &Path) -> Vec<u32> {
    let target = norm(&game_dir.to_string_lossy());
    let sys = refresh();
    sys.processes()
        .iter()
        .filter(|(_, p)| p.name().to_string_lossy().to_lowercase().contains("java"))
        .filter(|(_, p)| {
            let cwd_match = p.cwd().map(|c| norm(&c.to_string_lossy()) == target).unwrap_or(false);
            let arg_match = p.cmd().iter().any(|a| {
                let a = norm(&a.to_string_lossy());
                a == target || a.ends_with(&format!("={target}"))
            });
            cwd_match || arg_match
        })
        .map(|(pid, _)| pid.as_u32())
        .collect()
}

pub fn is_running(pid: u32) -> bool {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::Some(&[Pid::from_u32(pid)]), true);
    sys.process(Pid::from_u32(pid)).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_paths() {
        assert_eq!(norm("C:\\Games\\Pack\\"), norm("C:/Games/Pack"));
    }

    #[test]
    fn no_process_for_random_dir() {
        assert!(find_game_processes(Path::new("/definitely/not/a/game/dir")).is_empty());
    }
}
