from __future__ import annotations

from running_process import dashboard


def test_format_originator_splits_tool_and_pid() -> None:
    assert dashboard._format_originator("codeup:1234") == "codeup (1234)"
    assert dashboard._format_originator("plain-originator") == "plain-originator"
    assert dashboard._format_originator("") == "unknown"


def test_build_process_tree_nests_children_under_tracked_parent() -> None:
    processes = [
        {"pid": 10, "parent_pid": None, "created_at": 1.0, "registered_at": 1.0},
        {"pid": 11, "parent_pid": 10, "created_at": 2.0, "registered_at": 2.0},
        {"pid": 12, "parent_pid": 11, "created_at": 3.0, "registered_at": 3.0},
        {"pid": 99, "parent_pid": 5000, "created_at": 4.0, "registered_at": 4.0},
    ]

    tree = dashboard._build_process_tree(processes)

    assert [node["pid"] for node in tree] == [10, 99]
    assert [node["pid"] for node in tree[0]["children"]] == [11]
    assert [node["pid"] for node in tree[0]["children"][0]["children"]] == [12]


def test_dashboard_payload_enriches_processes_and_summary(monkeypatch) -> None:
    monkeypatch.setattr(
        dashboard,
        "_fetch_processes_json",
        lambda: [
            {
                "pid": 101,
                "state": 1,
                "kind": "subprocess",
                "command": "python parent.py",
                "cwd": "/repo",
                "originator": "agent:9000",
                "created_at": 1700000000.0,
                "registered_at": 1700000001.0,
            },
            {
                "pid": 102,
                "state": 1,
                "kind": "pty",
                "command": "python child.py",
                "cwd": "/repo",
                "originator": "",
                "created_at": 1700000002.0,
                "registered_at": 1700000003.0,
            },
        ],
    )
    monkeypatch.setattr(dashboard, "_fetch_parent_pids", lambda pids: {101: 9000, 102: 101})

    payload = dashboard._dashboard_payload()

    assert payload["summary"] == {"tracked": 2, "roots": 1}
    assert len(payload["processes"]) == 2
    assert payload["tree"][0]["pid"] == 101
    assert payload["tree"][0]["children"][0]["pid"] == 102
    assert payload["processes"][0]["spawned_by"] == "agent (9000)"
    assert payload["processes"][1]["spawned_by"] == "tracked pid 101"
    assert payload["processes"][0]["state_name"] == "alive"
