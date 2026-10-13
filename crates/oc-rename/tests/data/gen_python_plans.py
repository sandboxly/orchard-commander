"""Reference plans (new names, errors, notes, pending, can_run) of rename.plan for the Rust
port. Paths are local, so names fold case as on macOS: the Rust test runs on macOS only."""
import json, random, sys
from pathlib import Path
from orchard_commander import rename, vfs
from orchard_commander.rename import Item, Rule
random.seed(11)
NAMES = ["a.txt", "A.TXT", "b.txt", "c", "sub", "x.jpg", "X.jpg", "ä.txt", "Straße.md", "STRASSE.md", ".h", "con.txt"]
MASKS = ["[N]", "same", "[U][N]", "[L][N]", "[C]", "[N1]", "x/[N]", "", "[=artist]", "[=artist|u]", "[N", "[N]_[C]", ".", "b", "[F][N]", "SAME", "sub"]
cases = []
for k in range(800):
    items = []
    used = set()
    for _ in range(random.randint(1, 6)):
        folder = random.choice(["/nowhere/d1", "/nowhere/d2", "/nowhere/d1/sub"])
        name = random.choice(NAMES)
        if (folder, name) in used: continue
        used.add((folder, name))
        is_dir = name == "sub" or random.random() < 0.1
        meta = random.choice([None, {}, {"artist": "Q"}])
        items.append({"path": f"{folder}/{name}", "name": name, "is_dir": is_dir, "meta": meta})
    taken = {f"/nowhere/d1/{n}" for n in random.sample(NAMES, 4)} | {f"/nowhere/d2/{n}" for n in random.sample(NAMES, 3)}
    kw = dict(name_mask=random.choice(MASKS), ext_mask=random.choice(["[E]", "", "[U][E]"]), case=random.choice(["unchanged", "lower", "upper"]),
              search=random.choice(["", "a", "x|b"]), replace=random.choice(["", "b", "Y|a"]))
    its = [Item(vfs.local(i["path"]), i["name"], i["is_dir"], 1772874304.0, "P", "G", meta=i["meta"]) for i in items]
    taken_cf = {t.casefold() for t in taken}
    def exists(loc):
        return loc.path.casefold() in taken_cf  # macOS disks fold case
    p = rename.plan(its, Rule(**kw), exists=exists)
    cases.append({"items": items, "taken": sorted(taken), "rule": kw, "mask_error": p.mask_error,
                  "changes": [[c.new, c.error, c.note, c.pending] for c in p.changes], "can_run": p.can_run})
Path(sys.argv[1]).write_text("\n".join(json.dumps(c, ensure_ascii=False) for c in cases) + "\n", encoding="utf-8")
