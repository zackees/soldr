// Direct concrete-tree references outside soldr-platform are denied even in
// private code. The pure detector unit tests cover every concrete-tree name;
// this UI test proves the lint is wired into rustc for an ordinary test file.
fn uses_imp_tree() {
    let platform_imp = ();
}

fn main() {}
