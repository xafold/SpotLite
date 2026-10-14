#!/usr/bin/env python3
"""SpotLite's Linux test suite, end to end. It builds a test folder with every kind of file SpotLite reads (pictures, a
screenshot with words on it, a text PDF, a scanned PDF, txt/md/html/docx/odt/rtf, sound, video, a git repo of code),
indexes it with the engine into an index of its own (never the real one), and checks:

  unit       the engine's unit tests (cargo test)
  estimate   `spotlite estimate --json` counts every kind
  index      every file is read: done, or skipped for the reason expected
  search     meaning and words find the right file, page, moment and line
  ocr        the words in a screenshot and on a scanned page are searchable
  new-only   a new file is indexed, a changed one read again, a moved one only re-pathed, a deleted one dropped, and
             every other file left as it was (its vectors untouched)
  services   search-service and index-service over their JSON lines, download-model with the model already there
  code       the code index: git-ignored files left out, a name found where it's defined
  ui         (with --ui) the panel in a headless Weston: a search's results and preview, the "New files" button,
             and its size in every state (empty, results, weaker matches open, cleared, long names, nothing found, the
             code hint, one letter typed) at scales 1 and 2, a screenshot of each kept in ui-sizes/ (with --keep)

Usage: scripts/test.py [--ui] [--keep]    Needs: the engine built (cargo build --release in engine/), ffmpeg,
ImageMagick, git; for --ui the debug shell (cargo build in app/src-tauri) and weston. Tesseract for the OCR checks.
"""
import json, os, re, shutil, sqlite3, subprocess, sys, tempfile, time, zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SPOTLITE = ROOT / "engine/target/release/spotlite"
SHELL = ROOT / "app/src-tauri/target/debug/spotlite-app"
results = []


def check(group, name, ok, detail=""):
    results.append((group, name, bool(ok), detail))
    mark = "\033[32m✓\033[0m" if ok else "\033[31m✗\033[0m"
    print(f"  {mark} {name}" + (f"  — {detail}" if detail and not ok else ""), flush=True)
    return ok


def run(*args, timeout=900, **kw):
    return subprocess.run([str(a) for a in args], capture_output=True, text=True, timeout=timeout, **kw)


def spotlite(*args, timeout=900):
    return run(SPOTLITE, *args, timeout=timeout)


# MARK: The test folder

def text_pdf(path, lines):
    """A one-page PDF of text in Helvetica (enough characters to count as a text page, not a scan)."""
    escape = lambda s: s.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")
    stream = "BT /F1 12 Tf 60 780 Td 16 TL " + " ".join(f"({escape(l)}) '" for l in lines) + " ET"
    objects = ["<< /Type /Catalog /Pages 2 0 R >>",
               "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
               "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 4 0 R >> >> "
               "/Contents 5 0 R >>",
               "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
               f"<< /Length {len(stream)} >>\nstream\n{stream}\nendstream"]
    write_pdf(path, [o.encode() for o in objects])


def image_pdf(path, jpeg, width, height):
    """A one-page PDF that is only a picture: a scan."""
    data = Path(jpeg).read_bytes()
    stream = b"q 595 0 0 842 0 0 cm /Im0 Do Q"
    objects = [b"<< /Type /Catalog /Pages 2 0 R >>",
               b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
               b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /XObject << /Im0 4 0 R >> >> "
               b"/Contents 5 0 R >>",
               f"<< /Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceRGB "
               f"/BitsPerComponent 8 /Filter /DCTDecode /Length {len(data)} >>\nstream\n".encode() + data
               + b"\nendstream",
               f"<< /Length {len(stream)} >>\nstream\n".encode() + stream + b"\nendstream"]
    write_pdf(path, objects)


def write_pdf(path, objects):
    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for number, body in enumerate(objects, 1):
        offsets.append(len(out))
        out += f"{number} 0 obj\n".encode() + body + b"\nendobj\n"
    xref = len(out)
    out += f"xref\n0 {len(objects) + 1}\n0000000000 65535 f \n".encode()
    out += b"".join(f"{o:010d} 00000 n \n".encode() for o in offsets)
    out += f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    Path(path).write_bytes(out)


def docx(path, paragraphs):
    body = "".join(f"<w:p><w:r><w:t>{p}</w:t></w:r></w:p>" for p in paragraphs)
    with zipfile.ZipFile(path, "w") as z:
        z.writestr("[Content_Types].xml", '<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/'
                   'package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/>'
                   '<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-'
                   'officedocument.wordprocessingml.document.main+xml"/></Types>')
        z.writestr("word/document.xml", '<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/'
                   f'wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>')


def odt(path, paragraphs):
    body = "".join(f"<text:p>{p}</text:p>" for p in paragraphs)
    with zipfile.ZipFile(path, "w") as z:
        z.writestr(zipfile.ZipInfo("mimetype"), "application/vnd.oasis.opendocument.text")
        z.writestr("content.xml", '<?xml version="1.0"?><office:document-content xmlns:office="urn:oasis:names:tc:'
                   'opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">'
                   f"<office:body><office:text>{body}</office:text></office:body></office:document-content>")


def make_testbed(base):
    folders = {name: base / name for name in ["docs", "pictures", "media", "papers"]}
    for folder in folders.values():
        folder.mkdir(parents=True)
    docs, pictures, media, papers = (folders[n] for n in ["docs", "pictures", "media", "papers"])
    (docs / "notes.txt").write_text("Team notes. The quarterly budget meeting moved to Thursday afternoon. Bring the "
                                    "vendor invoices and the hiring plan for the design team.\n")
    (docs / "Quarterly budget planning meeting notes for the design and engineering teams, final revised version "
            "(2).txt").write_text("Budget planning notes for the design and engineering teams: headcount, tooling and the "
                                  "offsite in March.\n")
    # A name in a CV against documents about what it means: the one with the words comes first.
    (docs / "cv.md").write_text("# Asha Rai\n\n## Projects\n\nLoan Lens: reads bank statements and payslips with a "
                                "language model. Kanban board app for a school.\n\n## Skills\n\nPython, SQL, React.\n")
    (docs / "loan-terms.txt").write_text("Loan agreement. The lender advances the principal to the borrower, who repays "
                                         "it in monthly instalments with interest. The lender may charge a fee for late "
                                         "payment. Mortgage, credit and lending terms for the borrower and the lender.\n")
    (docs / "lease.md").write_text("# Apartment lease\n\n## Pets\n\nThe tenant may keep one cat or one small dog. Any "
                                   "other animal needs the landlord's written consent.\n\n## Rent\n\nRent is due on the "
                                   "first day of each month.\n")
    (docs / "recipe.html").write_text("<html><head><style>p{}</style></head><body><h1>Chocolate cake</h1><p>Melt the "
                                      "dark chocolate with butter, fold in the eggs and sugar, then bake for 35 minutes."
                                      "</p><script>var x = 1;</script></body></html>")
    # A file known by its name and folder ("pancakes recipes": Recipes/pancakes.txt) against one that only says the
    # words together.
    (docs / "Recipes").mkdir()
    (docs / "Recipes" / "pancakes.txt").write_text("Pancakes for four: mix two cups of flour, two eggs and a cup of "
                                                   "milk. Fry each one in butter until golden.\n")
    (docs / "shopping.txt").write_text("Shopping for Sunday: rice, soap, batteries, and flour for the pancakes recipes "
                                       "my sister sent.\n")
    docx(docs / "quote.docx", ["Quote for solar panel installation", "Twelve rooftop panels with an inverter and a "
                               "home battery, installed in two days."])
    odt(docs / "minutes.odt", ["Minutes of the community garden committee", "The committee agreed to plant tomatoes "
                               "and to fix the broken water tap near the tool shed."])
    (docs / "letter.rtf").write_text(r"{\rtf1\ansi{\fonttbl{\f0 Times;}}\f0 Letter of recommendation\par I recommend "
                                     r"Maria for the senior nurse position at the city hospital.\par}")
    run("convert", "-size", "640x640", "xc:white", "-fill", "red", "-draw", "circle 320,320 320,120",
        pictures / "red-circle.png")
    run("convert", "-size", "900x600", "gradient:#0b1a3a-#000000", "-fill", "#f6e27a", "-draw",
        "circle 700,150 700,80", pictures / "night-moon.png")
    run("convert", "-size", "120x120", "xc:orange", pictures / "tiny-icon.png")   # too small: skipped
    run("convert", "-size", "1280x720", "xc:#f4f4f4", "-font", "DejaVu-Sans", "-pointsize", "30", "-fill", "#202020",
        "-annotate", "+60+140", "error: externally-managed-environment", "-annotate", "+60+200",
        "This Python environment is managed by your system.", pictures / "Screenshot from 2026-10-14 10-00-00.png")
    run("convert", "-size", "1240x1754", "xc:white", "-font", "DejaVu-Sans", "-pointsize", "46", "-fill", "black",
        "-annotate", "+120+300", "INVOICE NUMBER K7Q2LM", "-annotate", "+120+400", "Total due: 480 EUR",
        base / "scan.jpg")
    image_pdf(papers / "scanned-invoice.pdf", base / "scan.jpg", 1240, 1754)
    text_pdf(papers / "volcano-guide.pdf", [
        "Volcano safety guide for hikers.", "If the volcano erupts, follow the marked evacuation routes downhill.",
        "Stay away from river valleys where mudflows travel fast.", "Cover your nose and mouth against ash.",
        "Shelters are at the north and east trailheads, each with water and first aid kits."])
    run("ffmpeg", "-v", "error", "-f", "lavfi", "-i", "smptebars=size=640x360:rate=25", "-f", "lavfi", "-i",
        "sine=frequency=440:sample_rate=16000", "-t", "12", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac",
        "-shortest", media / "color-bars.mp4")
    # A fractal from 6 s, after black, a keyframe each second: its best frame is a moment in (the panel opens it there).
    run("ffmpeg", "-v", "error", "-f", "lavfi", "-i", "color=black:size=640x360:rate=25:duration=6", "-f", "lavfi",
        "-i", "mandelbrot=size=640x360:rate=25", "-filter_complex",
        "[1]trim=duration=8,setpts=PTS-STARTPTS[m];[0][m]concat=n=2:v=1[v]", "-map", "[v]", "-c:v", "libx264",
        "-g", "25", "-pix_fmt", "yuv420p", media / "fractal.mp4")
    # One keyframe, at the start (as generated clips have): decoding only keyframes left it with no frames.
    run("ffmpeg", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25", "-t", "8", "-c:v", "libx264",
        "-g", "1000", "-pix_fmt", "yuv420p", media / "one-keyframe.mp4")
    run("ffmpeg", "-v", "error", "-f", "lavfi", "-i", "sine=frequency=330:duration=8", media / "tone.mp3")
    run("ffmpeg", "-v", "error", "-f", "lavfi", "-i", "anullsrc=r=16000:cl=mono", "-t", "5", media / "silence.wav")
    # Left out on purpose: a developer folder, and a code project's assets (only screenshots count inside one).
    (docs / "node_modules").mkdir()
    (docs / "node_modules" / "readme.txt").write_text("a dependency's notes")
    project = docs / "website"
    project.mkdir()
    (project / "package.json").write_text("{}")
    run("convert", "-size", "500x500", "xc:green", project / "hero.png")
    # The code index: a git repo with an ignored file and a secret.
    repo = base / "code" / "billing"
    repo.mkdir(parents=True)
    run("git", "init", "-q", repo)
    (repo / ".gitignore").write_text("generated.py\n")
    (repo / "retry.py").write_text('"""Calls with retries."""\nimport time\n\n\ndef retry_with_backoff(call, attempts=5):\n'
                                   '    """Retry a failed call, waiting twice as long after each failure."""\n'
                                   "    delay = 0.5\n    for attempt in range(attempts):\n        try:\n"
                                   "            return call()\n        except Exception:\n            time.sleep(delay)\n"
                                   "            delay *= 2\n    raise RuntimeError('gave up')\n")
    (repo / "invoice.js").write_text("export function totalWithTax(lines, rate) {\n  const net = lines.reduce((sum, l) =>"
                                     " sum + l.price * l.quantity, 0);\n  return net * (1 + rate);\n}\n")
    # "readme.md of billing": the repo's README.md (its name and folder), above a script that copies "README.md" to
    # ".../billing/" (the words together in its text).
    (repo / "README.md").write_text("# Billing\n\nThe billing service of the shop. Run deploy.sh to ship it.\n")
    (repo / "deploy.sh").write_text("#!/bin/sh\nset -e\nmkdir -p /opt/billing\ncp README.md /opt/billing/\n"
                                    "cp invoice.js retry.py /opt/billing/\n")
    (repo / "generated.py").write_text("TABLE = [1, 2, 3]\n")
    (repo / ".env").write_text("API_KEY=not-a-real-key\n")
    return folders, repo


# MARK: The checks

def search(index, query, n=5, kind=None):
    args = ["search", query, "-n", str(n), "--json", "--index", index] + (["--kind", kind] if kind else [])
    out = spotlite(*args)
    try:
        return json.loads(out.stdout)
    except json.JSONDecodeError:
        return []


def names(hits):
    return [Path(h["path"]).name for h in hits]


def file_rows(index):
    with sqlite3.connect(Path(index) / "index.sqlite") as db:
        return {Path(p).name: (state, at, segments) for p, state, at, segments in db.execute(
            "SELECT path, state, indexed_at, (SELECT COUNT(*) FROM segments s WHERE s.file = f.id) FROM files f")}


class Service:
    """A helper process speaking JSON lines."""

    def __init__(self, *args):
        self.process = subprocess.Popen([str(SPOTLITE), *map(str, args)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.DEVNULL, text=True, bufsize=1)
        self.next_id = 0

    def line(self, timeout=60):
        import select
        ready, _, _ = select.select([self.process.stdout], [], [], timeout)
        if not ready:
            return None
        line = self.process.stdout.readline()
        return json.loads(line) if line else None

    def ask(self, request, timeout=60):
        self.next_id += 1
        request["id"] = self.next_id
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
        deadline = time.time() + timeout
        while time.time() < deadline:
            message = self.line(deadline - time.time())
            if message is None:
                return None
            if message.get("id") == self.next_id:
                return message
        return None

    def close(self):
        try:
            self.process.stdin.close()
            self.process.wait(10)
        except Exception:
            self.process.kill()


def main():
    ui = "--ui" in sys.argv
    keep = "--keep" in sys.argv
    if not SPOTLITE.exists():
        sys.exit(f"build the engine first: {SPOTLITE}")
    base = Path(tempfile.mkdtemp(prefix="spotlite-test-"))
    index = str(base / "index")
    started = time.time()
    print(f"test folder: {base}")

    print("\nunit")
    out = run("cargo", "test", "--release", "--quiet", cwd=ROOT / "engine", timeout=1800)
    summary = [l for l in out.stdout.splitlines() if l.startswith("test result")]
    check("unit", f"engine unit tests ({summary[-1].split('.')[1].strip() if summary else '?'})",
          out.returncode == 0, out.stdout[-500:] + out.stderr[-500:])

    folders, repo = make_testbed(base)
    roots = [str(f) for f in folders.values()]

    print("\nestimate")
    out = spotlite("estimate", *roots, "--json", "--index", index)
    answers = [json.loads(l) for l in out.stdout.splitlines() if l.startswith("{")]
    kinds = {}
    for answer in answers:
        for kind, count in answer.get("files", {}).items():
            kinds[kind] = kinds.get(kind, 0) + count
    check("estimate", "one answer per folder", len(answers) == len(roots), out.stdout[-300:])
    expected = {"doc": 11, "image": 3, "screenshot": 1, "pdf": 2, "video": 3, "audio": 2}
    check("estimate", f"counts every kind {expected}", kinds == expected, str(kinds))

    print("\nindex")
    t = time.time()
    out = spotlite("index", *roots, "--index", index)
    seconds = time.time() - t
    check("index", f"indexes the folder ({seconds:.1f} s)", out.returncode == 0, out.stderr[-500:])
    rows = file_rows(index)
    states = {name: row[0] for name, row in rows.items()}
    done = ["notes.txt", "lease.md", "recipe.html", "quote.docx", "minutes.odt", "letter.rtf", "red-circle.png",
            "night-moon.png", "Screenshot from 2026-10-14 10-00-00.png", "scanned-invoice.pdf", "volcano-guide.pdf",
            "color-bars.mp4", "tone.mp3"]
    check("index", "every readable file is done", all(states.get(n) == "done" for n in done),
          str({n: states.get(n) for n in done if states.get(n) != "done"}))
    check("index", "a tiny picture and a silent recording are skipped",
          states.get("tiny-icon.png") == "skipped" and states.get("silence.wav") == "skipped",
          f"tiny-icon {states.get('tiny-icon.png')}, silence {states.get('silence.wav')}")
    check("index", "node_modules and a code project's pictures are left out",
          "readme.txt" not in rows and "hero.png" not in rows, str([n for n in ("readme.txt", "hero.png") if n in rows]))

    print("\nsearch")
    cases = [
        ("budget meeting moved to Thursday", "notes.txt", 1),
        ("can I keep a dog in the apartment", "lease.md", 1),
        ("chocolate cake recipe", "recipe.html", 1),
        ("solar panels on the roof", "quote.docx", 1),
        ("garden committee meeting", "minutes.odt", 1),
        ("recommendation for a nurse job", "letter.rtf", 1),
        ("what to do when a volcano erupts", "volcano-guide.pdf", 1),
        ("loan lens", "cv.md", 1),
        ("a red circle", "red-circle.png", 2),
        ("the moon in the night sky", "night-moon.png", 2),
        ("tv color bars test pattern", "color-bars.mp4", 3),
    ]
    for query, expected_name, depth in cases:
        hits = search(index, query)
        found = names(hits)[:depth]
        check("search", f"“{query}” → {expected_name}" + (f" (top {depth})" if depth > 1 else ""),
              expected_name in found, f"got {names(hits)[:3]}")
    hits = search(index, "volcano evacuation routes")
    check("search", "a PDF hit points at its page", hits and hits[0].get("loc") == 1.0 and hits[0]["segment"] == "page",
          str(hits[:1]))
    with sqlite3.connect(Path(index) / "index.sqlite") as db:
        (info,) = db.execute("SELECT info FROM files WHERE path LIKE '%one-keyframe.mp4'").fetchone()
    check("search", "a clip with one keyframe still gets frames", int(json.loads(info or "{}").get("frames", 0)) > 0,
          str(info))
    hits = search(index, "color bars", kind="video")
    check("search", "a video hit points at a moment", hits and hits[0].get("loc") is not None, str(hits[:1]))
    hits = search(index, "pancakes recipes")
    check("search", "“pancakes recipes” → Recipes/pancakes.txt (its name and folder), above the words in a text",
          names(hits)[:1] == ["pancakes.txt"], f"got {names(hits)[:3]}")
    hits = search(index, "zzqxv nothing like this exists")
    check("search", "nonsense finds nothing strong", all(h.get("z", 0) < 4 for h in hits), str(names(hits)))

    print("\nocr")
    if shutil.which("tesseract"):
        hits = search(index, "externally-managed-environment")
        check("ocr", "words in a screenshot are found", hits and "Screenshot" in names(hits)[0], str(names(hits)[:3]))
        hits = search(index, "K7Q2LM")
        check("ocr", "words on a scanned page are found", hits and names(hits)[0] == "scanned-invoice.pdf",
              str(names(hits)[:3]))
    else:
        check("ocr", "tesseract installed (sudo apt install tesseract-ocr)", False)

    print("\nnew-only")
    before = file_rows(index)
    time.sleep(1.1)
    (folders["docs"] / "new-trip.txt").write_text("Packing list for the Everest base camp trek: down jacket, water "
                                                 "filter, trekking poles and altitude medicine.\n")
    (folders["docs"] / "notes.txt").write_text("Team notes. The quarterly budget meeting moved to Friday morning.\n")
    (folders["docs"] / "lease.md").rename(folders["papers"] / "lease-2026.md")
    (folders["pictures"] / "night-moon.png").unlink()
    out = spotlite("index", *roots, "--index", index)
    sync = next((l for l in out.stdout.splitlines() if l.startswith("sync:")), "")
    check("new-only", "the sync sees 1 new · 1 changed · 1 moved · 1 removed",
          "1 new · 1 changed · 1 moved · 1 removed" in sync, sync)
    after = file_rows(index)
    untouched = [n for n in before if n not in ("notes.txt", "lease.md", "night-moon.png")]
    same = [n for n in untouched if after.get(n) == before[n]]
    check("new-only", f"the other {len(untouched)} files keep their vectors (not read again)",
          len(same) == len(untouched), str([n for n in untouched if n not in same]))
    check("new-only", "the moved file keeps its vectors under its new name",
          after.get("lease-2026.md", (None, None, None))[1:] == before["lease.md"][1:], str(after.get("lease-2026.md")))
    check("new-only", "the changed file is read again", after.get("notes.txt", (0, 0))[1] != before["notes.txt"][1])
    check("new-only", "the deleted file is gone", "night-moon.png" not in after)
    hits = search(index, "trekking to Everest base camp")
    check("new-only", "the new file is searchable", hits and names(hits)[0] == "new-trip.txt", str(names(hits)[:3]))

    print("\nservices")
    service = Service("search-service", *roots, "--index", index, "--encoder-idle", "30")
    ready = service.line(30)
    check("services", "search-service says ready", ready and ready.get("event") == "ready", str(ready))
    words = service.ask({"op": "search", "q": "choco", "meaning": False, "prefix_last": True, "limit": 5})
    check("services", "keyword search while typing (a prefix)", words and "recipe.html" in names(words["hits"]),
          str(words)[:200])
    service.ask({"op": "embed_ahead", "q": "a red circle"}, timeout=120)
    t = time.time()
    meaning = service.ask({"op": "search", "q": "a red circle", "limit": 5})
    took = (time.time() - t) * 1000
    check("services", f"meaning search with a warm encoder ({took:.0f} ms round trip)",
          meaning and meaning["meaning"] and "red-circle.png" in names(meaning["hits"])[:2], str(meaning)[:200])
    text = service.ask({"op": "text", "segment": meaning["hits"][0]["segment_id"]}) if meaning else None
    check("services", "text of a segment", text is not None and "text" in text, str(text)[:200])
    renders = {}
    for kind, path, loc in [("pdf", folders["papers"] / "volcano-guide.pdf", 1),
                            ("video", folders["media"] / "color-bars.mp4", 2),
                            ("image", folders["pictures"] / "red-circle.png", None)]:
        reply = service.ask({"op": "render", "path": str(path), "kind": kind, "loc": loc, "pixels": 256})
        renders[kind] = bool(reply and reply.get("image") and Path(reply["image"]).exists())
    check("services", "renders a PDF page, a video frame and a picture", all(renders.values()), str(renders))
    folders_reply = service.ask({"op": "folders", "roots": [roots[1]], "skipped": [], "types": []})
    narrowed = service.ask({"op": "search", "q": "chocolate cake recipe", "limit": 5})
    check("services", "\"folders\" narrows what's searched",
          folders_reply and narrowed and "recipe.html" not in names(narrowed["hits"]), str(narrowed)[:200])
    status = service.ask({"op": "status"})
    check("services", "status reports the encoder and vectors", status and status.get("encoder") == "ready",
          str(status))
    quit_reply = service.ask({"op": "quit"})
    check("services", "quit", quit_reply and quit_reply.get("ok"))
    service.close()

    (folders["media"] / "another-note.md").write_text("Notes about the lighthouse restoration project budget.\n")
    service = Service("index-service", *roots, "--index", index, "--backfill", "--linger", "1")
    events = []
    while True:
        message = service.line(120)
        if message is None:
            break
        events.append(message)
        if message.get("event") == "exit":
            break
    service.close()
    names_seen = [e.get("event") for e in events]
    sync_event = next((e for e in events if e.get("event") == "sync"), {})
    check("services", "index-service: sync, file, summary, idle, exit",
          all(n in names_seen for n in ["sync", "file", "summary", "idle", "exit"]), str(names_seen))
    check("services", "index-service indexes only the 1 new file", sync_event.get("added") == 1
          and [e.get("path", "").endswith("another-note.md") for e in events if e.get("event") == "file"] == [True],
          str(sync_event))
    out = spotlite("download-model", timeout=600)
    last = json.loads(out.stdout.strip().splitlines()[-1]) if out.stdout.strip() else {}
    check("services", "download-model with the model there: done, nothing downloaded",
          last.get("event") == "done" and '"from":"download"' not in out.stdout, out.stdout[-200:])

    print("\ncode")
    out = spotlite("index", "--code", str(repo.parent), "--index", index)
    check("code", "indexes a repo", out.returncode == 0, out.stderr[-300:])
    with sqlite3.connect(Path(index) / "code.sqlite") as db:
        code_files = {Path(p).name for (p,) in db.execute("SELECT path FROM files")}
    check("code", "git-ignored files and secrets are left out", "generated.py" not in code_files
          and ".env" not in code_files and {"retry.py", "invoice.js"} <= code_files, str(code_files))
    hits = search(index, "code: retry a failed call and wait longer each time")
    check("code", "finds code by what it does", hits and names(hits)[0] == "retry.py", str(names(hits)[:3]))
    hits = search(index, "code: retry_with_backoff")
    check("code", "a name opens where it's defined (line 5)", hits and hits[0].get("focus") == 5.0,
          str(hits[:1])[:200])
    for query in ("code: readme.md of billing", "code: readme billing"):
        hits = search(index, query)
        check("code", f"“{query}” → the repo's README.md, above a script that names it",
              names(hits)[:1] == ["README.md"], str(names(hits)[:3]))

    if ui:
        print("\nui")
        ui_checks(base, index, roots)

    passed = sum(1 for r in results if r[2])
    print(f"\n{passed} of {len(results)} checks passed in {time.time() - started:.0f} s")
    for group, name, ok, detail in results:
        if not ok:
            print(f"  FAILED [{group}] {name}: {detail[:300]}")
    if not keep:
        shutil.rmtree(base, ignore_errors=True)
    sys.exit(0 if passed == len(results) else 1)


def ui_checks(base, index, roots):
    if not SHELL.exists() or not shutil.which("weston") or not shutil.which("dbus-run-session"):
        check("ui", "debug shell, weston and dbus-run-session available", False, f"{SHELL} / weston")
        return
    socket = "wl-spotlite-suite"
    runtime = os.environ.get("XDG_RUNTIME_DIR", "/tmp")
    compositor = subprocess.Popen(["weston", "--backend=headless", "--renderer=pixman", "--width=1600",
                                   "--height=1000", f"--socket={socket}", "--debug"],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(50):
        if Path(runtime, socket).exists():
            break
        time.sleep(0.2)
    # The test bus starts no services: under a bus of its own the desktop's portals would be started for it, and in a
    # headless session they hang or crash (COSMIC's does), and GTK waits on them.
    bus_config = base / "test-bus.conf"
    bus_config.write_text('<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" '
                          '"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd"><busconfig><type>session</type>'
                          '<listen>unix:tmpdir=/tmp</listen><auth>EXTERNAL</auth><policy context="default">'
                          '<allow send_destination="*" eavesdrop="true"/><allow eavesdrop="true"/><allow own="*"/>'
                          '</policy></busconfig>')
    env = {**os.environ, "WAYLAND_DISPLAY": socket, "GDK_BACKEND": "wayland", "GTK_USE_PORTAL": "0"}
    env.pop("DISPLAY", None)
    base_args = [str(SHELL), "--helper", str(SPOTLITE), "--index", index, "--roots", ":".join(roots),
                 "--code-roots", str(base / "code"), "--settings", str(base / "shell-settings.json"), "--debug-quit"]

    def report(*extra, timeout=90):
        # A session bus of its own: the shell's single-instance check never meets a SpotLite the person is running (it
        # would hand over to that one, and the test would be testing it), and nothing of theirs is stopped.
        shell = subprocess.Popen(["dbus-run-session", f"--config-file={bus_config}", "--"] + base_args + list(extra),
                                 env=env, text=True,
                                 stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True)
        try:
            stdout, _ = shell.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(shell.pid, 15)
            stdout, _ = shell.communicate()
            return None, (stdout or "")[-300:]
        for line in stdout.splitlines():
            if line.startswith("{") and ('"phase"' in line or '"dig"' in line):
                return json.loads(line), ""
        return None, stdout[-300:]

    shown, detail = report("--debug-query", "a red circle", "--debug-show")
    check("ui", "the panel shows results for a search", shown and shown["groups"]
          and "red-circle.png" in shown["groups"][0]["rows"], str(shown or detail)[:300])
    check("ui", "the preview's picture loads", shown and shown.get("preview", {}).get("loaded") is True,
          str(shown and shown.get("preview")))
    # "Index Code" off: no code results, and the panel says code search is off.
    code_off = base / "code-off-settings.json"
    code_off.write_text('{"codeIndexOff": true}')
    off, detail = report("--debug-query", "code: retry a failed call", "--debug-show", "--settings", str(code_off))
    check("ui", "code index off: “code:” finds nothing and says code search is off", off and not off["groups"]
          and off.get("message") == "Code search is off", str(off or detail)[:300])
    (Path(roots[0]) / "ui-new-file.txt").write_text("A note about migrating the payroll system to the cloud.\n")
    dig, detail = report("--debug-index-new", "--debug-show")
    labels = (dig or {}).get("dig", [])
    print(f"    (the button said: {' → '.join(labels)})")
    check("ui", "“New files” looks, indexes the new file, and says so", dig and dig.get("state") == "done"
          and any(l.startswith("Looking") for l in labels) and labels[-1] == "1 new indexed", str(labels or detail)[:300])
    open_checks(base, env, base_args, bus_config, roots)
    compositor.terminate()

    # The panel's size in each state: the window as tall as the panel asks (just the field when nothing is typed, with
    # nothing of the last results under it), nothing sticking out of its area or overlapping.
    states = ("empty=|results=a red circle|copied=!copy|weaker=!weaker|cleared=|long-names=budget planning meeting notes for "
              "design and engineering teams|nothing=zzqxv qqqq wwvv|code-hint=code:|typing=b|cleared-again=")
    shots = base / "ui-sizes"
    shots.mkdir(exist_ok=True)
    for scale in (1, 2):
        socket = f"wl-spotlite-suite-x{scale}"
        compositor = subprocess.Popen(["weston", "--backend=headless", "--renderer=pixman", "--width=1600",
                                       "--height=1000", f"--scale={scale}", f"--socket={socket}", "--debug"],
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(50):
            if Path(runtime, socket).exists():
                break
            time.sleep(0.2)
        scaled_env = {**env, "WAYLAND_DISPLAY": socket}
        args = [str(SHELL), "--helper", str(SPOTLITE), "--index", index, "--roots", ":".join(roots),
                "--settings", str(base / "shell-settings.json"), "--debug-show", "--debug-sizes", states]
        shell = subprocess.Popen(["dbus-run-session", f"--config-file={bus_config}", "--"] + args, env=scaled_env,
                                 text=True, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True)
        seen = 0
        deadline = time.time() + 240
        for line in shell.stdout:
            if time.time() > deadline or '"sizes":"done"' in line:
                break
            if not line.startswith("{") or '"case"' not in line:
                continue
            state = json.loads(line)
            seen += 1
            before = set(shots.glob("wayland-screenshot*.png"))
            subprocess.run(["weston-screenshooter"], cwd=shots, env={**os.environ, "WAYLAND_DISPLAY": socket},
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
            for new in set(shots.glob("wayland-screenshot*.png")) - before:
                new.rename(shots / f"scale{scale}-{state['case']}.png")
            check("ui", f"size at scale {scale}, {state['case']}: panel {state['wants']} px (window {state['window'][1]})",
                  not state["problems"], "; ".join(state["problems"]))
            if state["case"] == "copied":
                # Headless Weston has no seat, so no clipboard: SpotLite says so and runs on (the next states report).
                check("ui", f"“Copy path” at scale {scale} without a clipboard says so, no crash",
                      "no clipboard" in (state.get("error") or ""), str(state)[:300])
        check("ui", f"size checks at scale {scale} all ran", seen == states.count("|") + 1, f"{seen} states reported")
        os.killpg(shell.pid, 15)
        compositor.terminate()


def open_checks(base, env, base_args, bus_config, roots):
    """Enter on a result opens it where it matched, in the default app for its kind, and "search:" opens Google in
    the default browser's main profile. The default apps are stand-ins that write down how they were started."""
    apps = base / "apps"
    bin_dir, share, config, record = apps / "bin", apps / "share", apps / "config", apps / "record"
    for folder in (bin_dir, share / "applications", config / "google-chrome" / "Profile 2",
                   config / "google-chrome" / "Profile 3", record):
        folder.mkdir(parents=True, exist_ok=True)
    (config / "google-chrome" / "Local State").write_text(
        json.dumps({"profile": {"last_used": "Profile 3", "profiles_order": ["Profile 2", "Profile 3"]}}))
    defaults = {"pdf": ("evince %U", ["application/pdf"]),
                "video": ("vlc --started-from-file %U", ["video/mp4"]),
                "code": ("code %F", ["text/x-python"]),
                "browser": ("google-chrome-stable --ozone-platform=x11 %U",
                            ["x-scheme-handler/https", "x-scheme-handler/http"])}
    mimeapps = ["[Default Applications]"]
    for name, (command, types) in defaults.items():
        program, *rest = command.split(" ")
        script = bin_dir / program
        script.write_text(f'#!/bin/sh\nprintf "%s\\n" "$@" > "{record}/{program}.args"\n')
        script.chmod(0o755)
        (share / "applications" / f"spotlite-test-{name}.desktop").write_text(
            f"[Desktop Entry]\nType=Application\nName=Test {name}\nExec={script} {' '.join(rest)}\n"
            f"MimeType={';'.join(types)};\n")
        mimeapps += [f"{kind}=spotlite-test-{name}.desktop" for kind in types]
    (config / "mimeapps.list").write_text("\n".join(mimeapps) + "\n")
    # Not XDG_DATA_HOME: SpotLite's model is found through it.
    test_env = {**env, "XDG_CONFIG_HOME": str(config),
                "XDG_DATA_DIRS": f"{share}:{env.get('XDG_DATA_DIRS', '/usr/local/share:/usr/share')}"}
    states = ("pdf=what to do when a volcano erupts|pdf-open=!open|video=a colorful mandelbrot fractal|"
              "video-open=!open|code=code: retry_with_backoff|code-open=!open|web=search: \"spotlite test\"|"
              "web-panel=!open|web-reshow=!reshow|web-outside=!open-outside|web-again=search: \"spotlite test\"|"
              "web-window=!open-window|files-again=a red circle|files-reshow=!reshow")
    socket = "wl-spotlite-suite-open"
    runtime = os.environ.get("XDG_RUNTIME_DIR", "/tmp")
    compositor = subprocess.Popen(["weston", "--backend=headless", "--renderer=pixman", "--width=1600",
                                   "--height=1000", f"--socket={socket}", "--debug"],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(50):
        if Path(runtime, socket).exists():
            break
        time.sleep(0.2)
    args = [a for a in base_args if a != "--debug-quit"] + ["--debug-show", "--debug-sizes", states]
    shell = subprocess.Popen(["dbus-run-session", f"--config-file={bus_config}", "--"] + args,
                             env={**test_env, "WAYLAND_DISPLAY": socket}, text=True, stdout=subprocess.PIPE,
                             stderr=open(base / "open-shell.err", "w"), start_new_session=True)
    buttons, webs, web_views = {}, {}, {}
    deadline = time.time() + 150
    for line in shell.stdout:
        if time.time() > deadline or '"sizes":"done"' in line:
            break
        if line.startswith("{") and '"case"' in line:
            state = json.loads(line)
            buttons[state["case"]] = state.get("open")
            webs[state["case"]] = state.get("web")
            web_views[state["case"]] = state.get("web_view")
    # SpotLite's browser runs on in a process of its own: up 2 s later (it didn't fail to start), then stopped.
    time.sleep(2)
    own_browser = f"^{re.escape(str(SHELL))} --browser https://www\\.google\\.com/search\\?q=spotlite\\+test"
    browser_up = subprocess.run(["pgrep", "-f", own_browser], capture_output=True).returncode == 0
    subprocess.run(["pkill", "-f", own_browser])
    os.killpg(shell.pid, 15)
    compositor.terminate()
    started = lambda program: (record / f"{program}.args").read_text().splitlines() \
        if (record / f"{program}.args").exists() else None
    pdf, video, code, browser = (started(p) for p in ("evince", "vlc", "code", "google-chrome-stable"))
    print(f"    (the Open buttons said: {buttons.get('pdf')} · {buttons.get('video')} · {buttons.get('code')})")
    check("ui", "a PDF opens in its default app at the page, finding a word from it",
          pdf and "--page-index=1" in pdf and any(a in ("--find=volcano", "--find=erupts") for a in pdf)
          and pdf[-1].endswith("volcano-guide.pdf") and buttons.get("pdf") == "Open at p. 1", f"{pdf} {buttons}")
    check("ui", "a video opens in its default app at the moment",
          video and video[0] == "--started-from-file" and any(a.startswith("--start-time=") and int(a[13:]) >= 4
                                                              for a in video)
          and video[-1].endswith("fractal.mp4") and (buttons.get("video") or "").startswith("Open at 0:0"),
          f"{video} {buttons}")
    check("ui", "code opens in its default editor at the line", code and code[0] == "-g"
          and code[1].endswith("retry.py:5") and buttons.get("code") == "Open at line 5", f"{code} {buttons}")
    check("ui", "“search:” with Ctrl+Enter opens Google in the default browser's main profile",
          browser and browser[:2] == ["--ozone-platform=x11", "--profile-directory=Profile 2"] and len(browser) == 3
          and re.match(r"https://www\.google\.com/search\?q=spotlite\+test", browser[2]), str(browser))
    said = (base / "open-shell.err").read_text(errors="replace")
    check("ui", "“search:” shows Google's results in the panel, and goes when a file search follows",
          webs.get("web-panel") is True and webs.get("files-again") is False, str(webs))
    # Shown again, the panel shows the web view only while "search:" is still typed (GTK's show_all() showed it always).
    check("ui", "“search:”'s web view comes back with the panel, and not after a file search",
          web_views.get("web-reshow") is True and web_views.get("files-again") is False
          and web_views.get("files-reshow") is False, str(web_views))
    check("ui", "“search:” with Shift+Enter opens the page in SpotLite's bigger browser, a process of its own",
          "search the web here" in said and browser_up, said[-300:])


if __name__ == "__main__":
    main()
