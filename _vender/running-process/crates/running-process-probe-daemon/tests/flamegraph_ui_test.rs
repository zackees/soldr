//! The flame-graph page (S16 / #645).
//!
//! Two things have to hold, and they are the two the issue names: the hot
//! frame must actually dominate the rendering, and the page must reference
//! nothing outside the daemon.

use running_process_probe_daemon::http::flamegraph::{render_html, FLAME_CSP};
use running_process_probe_daemon::profile::store::{collapsed_to_tree, FlameNode};

/// A profile where one stack is overwhelmingly hot.
const HOT: &str = include_str!("fixtures/hot.collapsed");

fn child<'a>(node: &'a FlameNode, name: &str) -> &'a FlameNode {
    node.children
        .iter()
        .find(|child| child.name == name)
        .unwrap_or_else(|| panic!("no child {name:?} under {:?}", node.name))
}

#[test]
fn the_hot_frame_is_the_widest_in_the_rendered_tree() {
    // Width in a flame graph is `value / root.value`, so "widest" is a
    // property of the folded tree rather than of the DOM — which is what makes
    // it assertable without a browser.
    let tree = collapsed_to_tree(HOT);
    assert_eq!(tree.value, 1000);

    let main = child(&tree, "main");
    assert_eq!(main.value, 1000);

    let hot = child(main, "spin_hot");
    assert_eq!(hot.value, 950);

    // Hottest sibling first, so the widest run is leftmost — a flame graph is
    // read by eye, and an operator's eye goes left.
    assert_eq!(main.children[0].name, "spin_hot");
    assert!(main.children[0].value > main.children[1].value);
}

#[test]
fn a_shared_prefix_is_merged_rather_than_duplicated() {
    // `setup` appears alone and as a prefix of `setup;read_config`. Two
    // separate `setup` boxes would misreport both.
    let tree = collapsed_to_tree(HOT);
    let main = child(&tree, "main");
    assert_eq!(
        main.children.iter().filter(|c| c.name == "setup").count(),
        1
    );
    let setup = child(main, "setup");
    assert_eq!(setup.value, 48, "40 alone plus 8 through read_config");
    assert_eq!(child(setup, "read_config").value, 8);
}

#[test]
fn a_malformed_line_is_skipped_rather_than_failing_the_render() {
    // A profile is a sampled artifact. Losing one line of it beats showing the
    // operator nothing at all.
    let tree = collapsed_to_tree("main;a 3\nno-count-here\nmain;b notanumber\n");
    assert_eq!(tree.value, 3);
    assert_eq!(tree.children.len(), 1);
}

#[test]
fn an_empty_profile_renders_as_an_empty_tree() {
    let tree = collapsed_to_tree("");
    assert_eq!(tree.value, 0);
    assert!(tree.children.is_empty());
}

#[test]
fn the_page_references_no_external_host() {
    // Asserted on the served bytes, not trusted to review: a stray CDN
    // `<script src>` added later would work perfectly on the author's laptop
    // and fail on every air-gapped host this tool exists to help with.
    let tree = collapsed_to_tree(HOT);
    let html = page_html(&tree);

    for needle in ["http://", "https://", "//cdn.", "@import url("] {
        assert!(
            !html.contains(needle),
            "the flame-graph page references an external resource ({needle})"
        );
    }
    // And the renderer and data really are in the document, so "no external
    // reference" is not merely "no content".
    assert!(html.contains("flame-frame"));
    assert!(html.contains("spin_hot"));
}

#[test]
fn the_content_security_policy_forbids_every_external_fetch() {
    // The structural half of self-containment: `default-src 'none'` makes an
    // accidental external reference a visible console error rather than a
    // silent dependency on someone else's server.
    assert!(FLAME_CSP.contains("default-src 'none'"));
    // Inline is required precisely because everything is inlined — there is no
    // external file to point a hash or nonce at.
    assert!(FLAME_CSP.contains("script-src 'unsafe-inline'"));
    assert!(!FLAME_CSP.contains("http"));
}

/// The exact markup the handler serves.
fn page_html(tree: &FlameNode) -> String {
    render_html(tree, "profile 1", "fixture")
}
