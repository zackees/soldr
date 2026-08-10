//! `GetProcessTree` handler — builds a human-readable tree display via
//! sysinfo.

use crate::proto::daemon::{DaemonRequest, DaemonResponse, GetProcessTreeResponse, StatusCode};
use sysinfo::{Pid, System};

use super::util::error_response;
use super::DaemonState;

/// Handle a `GetProcessTree` request by building a tree display string via sysinfo.
pub fn handle_get_process_tree(request: &DaemonRequest, _state: &DaemonState) -> DaemonResponse {
    let Some(ref req) = request.get_process_tree else {
        return error_response(
            request.id,
            StatusCode::InvalidArgument,
            "missing get_process_tree payload".into(),
        );
    };

    let tree_display = build_process_tree_display(req.pid);

    DaemonResponse {
        request_id: request.id,
        code: StatusCode::Ok as i32,
        message: String::new(),
        get_process_tree: Some(GetProcessTreeResponse { tree_display }),
        ..Default::default()
    }
}

/// Build a human-readable process tree string rooted at `root_pid` using sysinfo.
fn build_process_tree_display(root_pid: u32) -> String {
    let mut sys = System::new();
    sys.refresh_processes();

    let sysinfo_pid = Pid::from_u32(root_pid);
    let Some(root_proc) = sys.process(sysinfo_pid) else {
        return format!("Process {root_pid} not found");
    };

    let mut lines = Vec::new();
    lines.push(format!(
        "{} (pid={root_pid}) {}",
        root_proc.name(),
        root_proc
            .cmd()
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    ));

    // Collect children recursively.
    fn collect_children(sys: &System, parent_pid: Pid, prefix: &str, lines: &mut Vec<String>) {
        let children: Vec<_> = sys
            .processes()
            .values()
            .filter(|p| p.parent() == Some(parent_pid))
            .collect();

        for (i, child) in children.iter().enumerate() {
            let is_last = i == children.len() - 1;
            let connector = if is_last { "└── " } else { "├── " };
            let child_prefix = if is_last { "    " } else { "│   " };

            lines.push(format!(
                "{prefix}{connector}{} (pid={})",
                child.name(),
                child.pid().as_u32()
            ));

            collect_children(sys, child.pid(), &format!("{prefix}{child_prefix}"), lines);
        }
    }

    collect_children(&sys, sysinfo_pid, "", &mut lines);
    lines.join("\n")
}
