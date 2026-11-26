use super::*;

/// A bind holds its child's slot for its whole scan. A watcher batch for
/// another child, already bound, must be recorded without waiting for it.
#[test]
fn a_child_bind_in_progress_does_not_hold_up_another_childs_watcher_paths() {
    use crate::data_layer::checkout::Planes;
    use crate::data_layer::runtime::Fresh;

    let temp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let parent = base.join("projects");
    let storage = base.join("storage");
    std::fs::create_dir_all(&storage).unwrap();
    let binding = parent.join("binding");
    let bound = parent.join("bound");
    write(&binding, "src/lib.rs", "pub fn binding() {}\n");
    write(&bound, "src/lib.rs", "pub fn bound() {}\n");
    let children = Arc::new(ChildCheckouts::default());
    let bound_slot = children.slot(&parent, &bound);
    let checkout = bind(&bound_slot, &bound, &storage, None, Planes::ALL).unwrap();
    assert!(matches!(
        checkout.fresh_view(Duration::from_secs(60)),
        Fresh::Ready(_)
    ));
    assert_eq!(checkout.checkout().changed_paths(), 0);

    let binding_slot = children.slot(&parent, &binding);
    let bind_in_progress = binding_slot.lock();
    let edited = write(&bound, "src/added.rs", "pub fn added() {}\n");
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let noter = {
        let children = Arc::clone(&children);
        std::thread::spawn(move || {
            children.note_paths(&[edited]);
            done_tx.send(()).unwrap();
        })
    };
    // A bound on failure only: a passing run is answered at once.
    let finished = done_rx.recv_timeout(Duration::from_secs(30));
    drop(bind_in_progress);
    noter.join().unwrap();
    assert!(
        finished.is_ok(),
        "a watcher path for a bound child waited for another child's bind"
    );
    assert_eq!(checkout.checkout().changed_paths(), 1);
}

fn write(root: &Path, path: &str, text: &str) -> PathBuf {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}
