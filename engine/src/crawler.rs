//! Walks the chosen folders and applies `rules`. Reads metadata only, never file contents (except the 600-byte sniff
//! that tells a real MPEG-TS video from a TypeScript file). For the code index it walks the code folders instead: code
//! files only, in a git repo only those git keeps, and none in vendored or built folders.
use crate::model::{FileKind, IndexOptions};
use crate::paths;
use crate::rules::{self, Classification};
use crate::store::Candidate;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Default, Debug)]
pub struct CrawlReport {
    pub candidates: Vec<Candidate>,
    /// Reason → files.
    pub skipped: BTreeMap<String, usize>,
    /// Reason → folders not walked into.
    pub skipped_folders: BTreeMap<String, usize>,
    /// Folder → files skipped as a dataset.
    pub datasets: HashMap<String, usize>,
}

/// A file's own facts, without following a link. None when it isn't there.
pub fn file_info(path: &str) -> Option<(bool, i64, f64, u64, u64)> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let mtime = meta.mtime() as f64 + meta.mtime_nsec() as f64 / 1e9;
    Some((meta.file_type().is_file(), meta.len() as i64, mtime, meta.ino(), meta.dev()))
}

pub fn crawl(roots: &[String], options: &IndexOptions) -> CrawlReport {
    let home = paths::home_string();
    let mut report = CrawlReport::default();
    let mut project_roots: HashSet<String> = HashSet::new();
    let mut repos = GitRepos::default();
    let mut found: Vec<Candidate> = vec![];
    let mut found_paths: HashSet<String> = HashSet::new();   // a folder inside another root would be walked twice
    let root_paths: HashSet<String> = roots.iter().cloned().collect();

    for root in roots {
        if options.code {
            repos.enter_root(root);
        } else if rules::is_project_root(root) {
            project_roots.insert(root.clone());
        }
        // A skip applies inside the chosen folders that contain it: a folder chosen inside a skipped one is walked
        // whole. The walk reports paths as the root spells them.
        let real = paths::real_path(root);
        let skips: Vec<String> = options.excluded_folders.iter()
            .filter(|skip| rules::is_inside(skip, root) || rules::is_inside(skip, &real))
            .flat_map(|skip| [skip.clone(), paths::real_path(skip)]).collect();
        let mut stack = vec![root.clone()];
        while let Some(folder) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&folder) else { continue };
            for entry in entries.flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    *report.skipped.entry("name that isn't UTF-8".into()).or_default() += 1;
                    continue;
                };
                if name.starts_with('.') {
                    continue;   // hidden
                }
                let path = if folder.ends_with('/') { format!("{folder}{name}") } else { format!("{folder}/{name}") };
                let Ok(file_type) = entry.file_type() else { continue };
                if file_type.is_dir() {
                    if rules::is_package_folder(&path) {
                        *report.skipped.entry("app or bundle".into()).or_default() += 1;
                    } else if let Some(reason) = rules::skip_reason(&path, &home, &skips)
                        .or_else(|| if options.code { rules::code_skip_reason(&path) } else { None }) {
                        *report.skipped_folders.entry(reason.into()).or_default() += 1;
                    } else {
                        if options.code {
                            repos.enter(&path);
                        } else if rules::is_project_root(&path) {
                            project_roots.insert(path.clone());
                        }
                        stack.push(path);
                    }
                    continue;
                }
                let classification = if options.code { rules::classify_code(&path, options) } else {
                    rules::classify(&path, options)
                };
                match classification {
                    Classification::Ignore => *report.skipped.entry("other type".into()).or_default() += 1,
                    Classification::Skip(reason) => *report.skipped.entry(reason.into()).or_default() += 1,
                    Classification::Index(kind) => {
                        let Some((true, size, mtime, inode, device)) = file_info(&path) else {
                            *report.skipped.entry("link or special file".into()).or_default() += 1;
                            continue;
                        };
                        if options.code {
                            // Too big to be written by hand; a notebook's file holds its outputs too.
                            let limit = if paths::extension(&path) == "ipynb" { options.max_notebook_bytes } else {
                                options.max_code_bytes
                            };
                            if size as u64 > limit {
                                *report.skipped.entry("too big to be written by hand".into()).or_default() += 1;
                                continue;
                            }
                            if repos.ignores(&path) {
                                *report.skipped.entry("ignored by git".into()).or_default() += 1;
                                continue;
                            }
                        }
                        if found_paths.insert(path.clone()) {
                            found.push(Candidate { path, kind, size, mtime, inode, device });
                        }
                    }
                }
            }
        }
    }

    // Inside a code project only screenshots count (assets, fixtures and datasets don't).
    let mut in_project: HashMap<String, bool> = HashMap::new();
    fn is_in_project(folder: &str, roots: &HashSet<String>, known: &mut HashMap<String, bool>) -> bool {
        if let Some(&answer) = known.get(folder) {
            return answer;
        }
        let parent = paths::parent(folder);
        let answer = roots.contains(folder) || (parent != folder && parent.len() > 1 && is_in_project(parent, roots, known));
        known.insert(folder.to_string(), answer);
        answer
    }
    let mut kept = vec![];
    for file in found {
        if !options.code && file.kind != FileKind::Screenshot
            && is_in_project(paths::parent(&file.path), &project_roots, &mut in_project) {
            *report.skipped.entry("inside a code project".into()).or_default() += 1;
        } else {
            kept.push(file);
        }
    }

    if !options.include_datasets && !options.code {
        let files: Vec<(String, String, FileKind)> = kept.iter()
            .map(|f| (paths::parent(&f.path).to_string(), paths::stem(&f.path).to_string(), f.kind)).collect();
        let datasets = rules::dataset_folders(&files, &root_paths);
        if !datasets.is_empty() {
            kept.retain(|file| {
                let skip = datasets.contains_key(paths::parent(&file.path))
                    && matches!(file.kind, FileKind::Image | FileKind::Audio | FileKind::Video);
                if skip {
                    *report.skipped.entry("dataset".into()).or_default() += 1;
                }
                !skip
            });
            report.datasets = datasets;
        }
    }
    report.candidates = kept;
    report
}

/// The git repos a code crawl walks through, and the files each keeps: a file git ignores (a build, an environment,
/// data, a secret listed in .gitignore) isn't code search's to read. A folder that isn't in a repo, or a machine
/// without git, has no list: everything there counts.
#[derive(Default)]
struct GitRepos {
    /// Repo folder → the files git keeps in it (relative to it), or None when git couldn't say.
    files: HashMap<String, Option<HashSet<String>>>,
    /// Folder → the repo it's in (None: none), as the walk found them.
    repo_of: HashMap<String, Option<String>>,
}

impl GitRepos {
    /// A chosen folder: a repo itself, or inside one.
    fn enter_root(&mut self, path: &str) {
        let mut folder = path.to_string();
        loop {
            if Path::new(&format!("{folder}/.git")).exists() {
                self.files.insert(folder.clone(), git_files(&folder));
                self.repo_of.insert(path.to_string(), Some(folder));
                return;
            }
            let parent = paths::parent(&folder).to_string();
            if parent == folder || parent.len() <= 1 {
                break;
            }
            folder = parent;
        }
        self.repo_of.insert(path.to_string(), None);
    }

    /// A folder the walk goes into: a repo of its own when it has a .git folder.
    fn enter(&mut self, path: &str) {
        if Path::new(&format!("{path}/.git")).is_dir() {
            self.files.insert(path.to_string(), git_files(path));
            self.repo_of.insert(path.to_string(), Some(path.to_string()));
        }
    }

    fn repo(&mut self, folder: &str) -> Option<String> {
        if let Some(known) = self.repo_of.get(folder) {
            return known.clone();
        }
        let parent = paths::parent(folder);
        let answer = if parent != folder && parent.len() > 1 { self.repo(&parent.to_string()) } else { None };
        self.repo_of.insert(folder.to_string(), answer.clone());
        answer
    }

    /// Whether git ignores the file at `path` (in a repo whose files git listed).
    fn ignores(&mut self, path: &str) -> bool {
        let Some(repo) = self.repo(paths::parent(path)) else { return false };
        let Some(Some(kept)) = self.files.get(&repo) else { return false };
        !kept.contains(&path[repo.len() + 1..])
    }
}

/// The files git keeps in a repo: tracked ones, and new ones its .gitignore doesn't leave out. None when git can't say.
fn git_files(repo: &str) -> Option<HashSet<String>> {
    let output = std::process::Command::new("git")
        .args(["-C", repo, "ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        // Reading only: never wait for, or take, the repo's lock (an editor's git may be busy in it).
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output().ok()?;
    output.status.success().then(|| {
        output.stdout.split(|&b| b == 0).filter(|p| !p.is_empty())
            .filter_map(|p| std::str::from_utf8(p).ok().map(str::to_string)).collect()
    })
}
