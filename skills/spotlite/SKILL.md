---
name: spotlite
description: Search this user's own files on this machine by content and meaning with the `spotlite` command (SpotLite, local, GPU). Use when the user asks to find something they have — a document, a PDF page, a screenshot, a photo, a video moment, a recording, or their code ("find the PDF about X", "the screenshot with that error", "where did I save the invoice", "the video where…") — or when a task needs facts from their own documents. Not for web search, and not for files of the repo you are working in (use Grep/Glob there).
---

# spotlite: search the user's files

SpotLite indexes the folders the user chose: in the SpotLite app's setup, or with `spotlite index <folders>` when only
the command is installed (`spotlite status` shows what is in the index). It reads PDFs page by page (scans through
OCR), Word/ODT/RTF/text/Markdown/HTML, screenshots and the words on them, photos, video frames and sound, and audio.
Code is in a separate index, filled only from the code folders the user chose (the app's tray: Code Folders…, or
`spotlite index --code <folder>`). With none chosen, `code:` finds nothing.

## Commands

```bash
spotlite search "what it is about, or words in it" -n 10      # ranked results
spotlite search "dentist invoice" --kind pdf,doc -n 5         # kinds: screenshot, image, pdf, doc, audio, video
spotlite search "how long the bus takes" -n 3 --text          # each hit's matched text (a whole PDF page, the lines)
spotlite search "latency chart" --json                        # every field, for scripts
spotlite search "code: retry a failed call with backoff"      # the code index
spotlite status                                               # what is indexed
```

Output: one hit per block. The first line has the rank, kind, full path, the place in brackets (`p. 12`, `at 1:05`,
`lines 5–40, opens at 5`, `text in image`) and why it matched: `z 6.9` is meaning (2 or more is a real match),
`words #3` is the keyword rank. Results go to stdout. The model's two log lines go to stderr.

`--json` fields: `path`, `kind`, `segment` (page, page_image, frame, audio, chunk, lines, ocr, image, name), `loc`
(page number, seconds, or first line), `loc_end`, `focus` (code line to open at), `z`, `keyword_rank`, `excerpt`
(found words in «»), and `text` with `--text`.

## How to search well

- Describe the content, not the file name: "graph of loss over epochs", "Nepal citizenship form", "red car at night".
  Meaning search covers pictures and video frames as well as text.
- One search takes about 1 second (the model loads on the GPU each time). Prefer one good query with `--text` and a
  small `-n` to many small queries.
- Hits far below the top one (low z, no words) are usually noise. Say so instead of presenting them as answers.

## Reading a hit

- Text, Markdown, code: the Read tool on the path (code at `focus`).
- PDF page P: `--text`, or `pdftotext -f P -l P -layout "<path>" -`.
- Screenshot or photo: the Read tool on the path shows you the picture.
- Video at second S: `ffmpeg -v error -ss S -i "<path>" -frames:v 1 <scratchpad>/frame.jpg`, then Read the frame.

## Rules

- Read only. Do not run `spotlite index`, `--reindex` or `index-service` unless the user asks. With the SpotLite app
  installed (`~/.local/lib/spotlite/spotlite-app` exists), the app keeps the index current. With only the command
  installed, the user runs `spotlite index <folders>` to add new files: suggest it when a search misses a file they
  just added.
- An empty index (`spotlite status` shows no files): the model and the index are not set up yet. Tell the user to run
  `spotlite download-model` once, then `spotlite index <their folders>`.
- Do not open apps on the user's screen unless they ask. If they ask, `xdg-open "<path>"` opens the file in their
  default app.
- The files are private. Do not send their contents to outside services.
- `spotlite: command not found`: the program is `~/.local/lib/spotlite/spotlite`. To install SpotLite, or only the
  command: https://github.com/xafold/SpotLite#install.
