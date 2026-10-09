"""Contract checks for `vfs.FileSystem` implementations.

Usable by built-in tests and by add-on authors:

    from orchard_commander.testing import SAMPLE_TREE, check_filesystem

    def test_my_filesystem(tmp_path):
        root = vfs.VPath(MyFileSystem(...), "/some/empty/dir")
        check_filesystem(root)                       # writable: writes SAMPLE_TREE itself

    def test_my_readonly_filesystem():
        root = vfs.VPath(MyReadOnlyFS(...), "")      # must already contain SAMPLE_TREE
        check_filesystem(root, writable=False)

The checks only use the generic interface, so they hold for any backend.
"""

from __future__ import annotations

import hashlib

from orchard_commander import fs, vfs

# relative path -> contents; directories are implied by the paths
SAMPLE_TREE: dict[str, bytes] = {
    "hello.txt": b"hello world\n",
    "empty.bin": b"",
    "docs/readme.md": b"# Readme\n",
    "docs/deep/data.bin": bytes(range(256)) * 300,  # 76.8 KB: more than one read chunk on most backends
    ".hidden": b"dot file\n",
}


def write_tree(root: vfs.VPath, tree: dict[str, bytes] = SAMPLE_TREE) -> None:
    """Create `tree` below `root` using only the file system's write API."""
    for rel, data in tree.items():
        parts = rel.split("/")
        if len(parts) > 1:
            root.fs.mkdir(root.child(*parts[:-1]).path, exist_ok=True)
        with root.fs.open_write(root.child(*parts).path) as f:
            f.write(data)


def check_filesystem(root: vfs.VPath, *, writable: bool = True, tree: dict[str, bytes] = SAMPLE_TREE) -> None:
    """Run the whole contract. For writable file systems `root` must be an empty
    directory (the sample tree is written into it); otherwise it must already
    contain `tree`."""
    assert root.fs.writable == writable, f"{root.fs.type_name}: writable should be {writable}"
    if writable:
        write_tree(root, tree)
    check_reading(root, tree)
    if writable:
        check_writing(root)
    else:
        check_read_only(root)


def check_reading(root: vfs.VPath, tree: dict[str, bytes] = SAMPLE_TREE) -> None:
    filesystem = root.fs
    assert root.is_dir() and root.exists()

    # Listing: names, kinds, sizes; hidden entries filtered on request.
    top_files = {rel for rel in tree if "/" not in rel}
    top_dirs = {rel.split("/")[0] for rel in tree if "/" in rel}
    entries = {e.name: e for e in root.list_dir(show_hidden=True)}
    assert set(entries) == top_files | top_dirs, f"listing: {sorted(entries)}"
    for name in top_dirs:
        assert entries[name].is_dir, f"{name} should be a directory"
    for name in top_files:
        assert not entries[name].is_dir and entries[name].size == len(tree[name]), f"{name}: size"
    visible = {e.name for e in root.list_dir(show_hidden=False)}
    assert all(not n.startswith(".") for n in visible), "hidden entries listed with show_hidden=False"
    listing = root.list_dir(show_hidden=True)
    assert [e.is_dir for e in listing] == sorted((e.is_dir for e in listing), reverse=True), "dirs first"

    for rel, data in tree.items():
        path = root.child(*rel.split("/"))
        # Paths: join / parent / basename agree.
        assert path.name == rel.split("/")[-1], f"basename of {rel}"
        assert path.parent() == root.child(*rel.split("/")[:-1]) if "/" in rel else path.parent() == root
        assert isinstance(path.display, str) and path.display
        # Metadata.
        entry = path.stat()
        assert path.exists() and not path.is_dir() and entry.size == len(data), f"stat {rel}"
        # Contents: streamed reads (in several chunks) and a local copy.
        with filesystem.open_read(path.path) as f:
            chunks = []
            while chunk := f.read(10_000):
                chunks.append(chunk)
        assert b"".join(chunks) == data, f"contents of {rel}"
        assert path.materialize().read_bytes() == data, f"materialize {rel}"

    missing = root.child("does-not-exist")
    assert not missing.exists() and not missing.is_dir()
    try:
        missing.stat()
    except FileNotFoundError:
        pass
    else:
        raise AssertionError("stat() of a missing path must raise FileNotFoundError")

    # walk(): the start first, parents before children, every file once.
    walked = list(filesystem.walk(root.child("docs").path))
    assert walked[0][0] == () and walked[0][1].is_dir
    seen_dirs = {()}
    for rel, entry in walked[1:]:
        assert rel[:-1] in seen_dirs, f"walk: {rel} before its parent"
        if entry.is_dir:
            seen_dirs.add(rel)
    walked_files = {"/".join(("docs", *rel)) for rel, e in walked if not e.is_dir}
    assert walked_files == {rel for rel in tree if rel.startswith("docs/")}


def check_writing(root: vfs.VPath) -> None:
    filesystem = root.fs
    new_dir = root.child("new", "nested")
    filesystem.mkdir(new_dir.path)  # creates missing parents
    assert new_dir.is_dir()
    filesystem.mkdir(new_dir.path, exist_ok=True)
    try:
        filesystem.mkdir(new_dir.path)
    except FileExistsError:
        pass
    else:
        raise AssertionError("mkdir of an existing directory must raise FileExistsError")

    target = new_dir.child("file.bin")
    payload = hashlib.sha256(b"payload").digest() * 1000
    with filesystem.open_write(target.path) as f:
        f.write(payload)
    with filesystem.open_read(target.path) as f:
        assert f.read() == payload
    with filesystem.open_write(target.path) as f:  # overwrite truncates
        f.write(b"short")
    assert target.stat().size == 5

    before = target.stat().mtime
    filesystem.set_mtime(target.path, 1_600_000_000.0)  # best effort: not every server can
    mtime = target.stat().mtime
    assert mtime in (0, before) or abs(mtime - 1_600_000_000.0) < 2, f"set_mtime: {mtime}"

    if filesystem.can_rename:
        renamed = root.child("new", "renamed.bin")
        filesystem.rename(target.path, renamed.path)
        assert renamed.exists() and not target.exists()

    filesystem.remove(root.child("new").path)  # whole directories
    assert not root.child("new").exists()
    filesystem.remove(root.child("hello.txt").path)
    assert not root.child("hello.txt").exists()


def check_read_only(root: vfs.VPath) -> None:
    filesystem = root.fs
    for attempt in (
        lambda: filesystem.mkdir(root.child("x").path),
        lambda: filesystem.open_write(root.child("x").path),
        lambda: filesystem.remove(root.child("hello.txt").path),
    ):
        try:
            attempt()
        except vfs.ReadOnlyError:
            continue
        except fs.OperationError:
            continue
        raise AssertionError("writes on a read-only file system must raise ReadOnlyError")
