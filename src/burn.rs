//! Which projects are eating the week, read from the local transcripts.
//!
//! Claude Code appends one JSON object per line to
//! `~/.claude/projects/<encoded-cwd>/<session>.jsonl`, and every assistant
//! message carries a `usage` block and a `cwd`. That is enough to say "this
//! week went mostly into dotnet-saas" without asking anyone anything.
//!
//! Two things this is careful about:
//!
//! **Cost.** The corpus here is tens of megabytes and grows forever. Re-parsing
//! it on a timer would make a tray app that idles at 2 MB and no measurable CPU
//! into one that does not, which is the whole thing this app is supposed to be.
//! So the files are treated as what they are — append-only logs — and each is
//! read from a remembered byte offset. The first scan reads everything once on
//! the background thread; every scan after that reads only what was appended.
//!
//! **Honesty.** The weights below are a cost-shaped proxy, not the real thing.
//! Anthropic does not publish how a subscription bucket converts tokens into a
//! percentage, and it certainly is not uniform across models. These numbers are
//! only ever shown as a *share between projects*, labelled as an estimate, and
//! never as a percentage of any limit. The limit numbers come from the server
//! and nowhere else — see `state.rs` for why that separation matters.

use crate::creds;
use crate::log::{ldebug, linfo};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Token weights, in input-token-equivalents, from the published Opus price
/// ratios ($5 in / $25 out per MTok, cache reads at a tenth, writes at 1.25x).
/// Applied uniformly across models on purpose: a per-model table would look
/// more precise without being more correct, since the bucket weighting is not
/// the price list.
const W_INPUT: f64 = 1.0;
const W_CACHE_WRITE: f64 = 1.25;
const W_CACHE_READ: f64 = 0.1;
const W_OUTPUT: f64 = 5.0;

const DAY: u64 = 86_400;
/// Matches the longest limit window the endpoint reports.
const RETAIN_DAYS: u64 = 7;

#[derive(Clone, Debug)]
pub struct Share {
    pub project: String,
    /// Fraction of the retained window's weighted tokens, 0.0..=1.0.
    pub fraction: f64,
}

/// Incremental reader over the transcript corpus.
///
/// Lives on the poller thread and is never shared; no locking needed.
#[derive(Default)]
pub struct Attribution {
    /// How far into each file we have already read.
    cursors: HashMap<PathBuf, u64>,
    /// (day since epoch, project) -> weighted tokens. Bucketing by day is what
    /// lets old usage age out of a running total we never fully recompute.
    buckets: HashMap<(u64, String), f64>,
    scanned_once: bool,
}

impl Attribution {
    /// Read whatever is new and return the current ranking, highest first.
    pub fn refresh(&mut self, now: u64) -> Vec<Share> {
        let Some(root) = creds::claude_dir().map(|d| d.join("projects")) else {
            return Vec::new();
        };
        if !root.is_dir() {
            return Vec::new();
        }

        let started = std::time::Instant::now();
        let mut files = 0usize;
        let mut bytes = 0u64;
        for path in jsonl_files(&root) {
            match self.ingest(&path, now) {
                Ok(n) if n > 0 => {
                    files += 1;
                    bytes += n;
                }
                Ok(_) => {}
                Err(e) => ldebug!("could not read {}: {e}", path.display()),
            }
        }

        let cutoff = now.saturating_sub(RETAIN_DAYS * DAY) / DAY;
        self.buckets.retain(|(day, _), _| *day >= cutoff);

        if !self.scanned_once {
            self.scanned_once = true;
            linfo!(
                "transcript baseline: {files} files, {} MB, {} ms",
                bytes / 1_048_576,
                started.elapsed().as_millis()
            );
        } else if bytes > 0 {
            ldebug!("transcripts: +{bytes} bytes across {files} files");
        }

        self.ranking()
    }

    /// Bytes newly consumed from this file.
    fn ingest(&mut self, path: &Path, now: u64) -> std::io::Result<u64> {
        let len = std::fs::metadata(path)?.len();
        let cursor = self.cursors.entry(path.to_path_buf()).or_insert(0);

        // A file that shrank was rotated or rewritten; start it over rather
        // than seeking past the end and reading nothing forever.
        if len < *cursor {
            *cursor = 0;
        }
        if len == *cursor {
            return Ok(0);
        }

        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(*cursor))?;
        let mut reader = BufReader::new(file);

        // `read_until` rather than `lines()`, because the cursor has to be an
        // exact byte count and `lines()` only hands back the text.
        //
        // Reconstructing the count as `line.len() + 1` is wrong twice over. It
        // undercounts CRLF, which `lines()` strips whole. And on a file whose
        // last line has no terminator yet — which is what every transcript looks
        // like while Claude Code is mid-write, and this scans every five minutes
        // — it *overcounts* by one, pushing the cursor past the end of the file.
        // The next scan then sees `len < cursor`, concludes the file was
        // rotated, rewinds to zero and re-reads the lot, adding a second copy of
        // every token in it to the totals.
        let mut buf = Vec::new();
        let mut consumed = 0u64;
        loop {
            buf.clear();
            let n = reader.read_until(b'\n', &mut buf)?;
            if n == 0 {
                break;
            }
            // No terminator means the writer is still working on this line.
            // Leave the cursor in front of it and pick it up whole next time.
            if !buf.ends_with(b"\n") {
                break;
            }
            consumed += n as u64;

            // Cheap gate before the expensive parse: most lines in a transcript
            // are user turns and tool results, and the big ones especially so.
            let line = String::from_utf8_lossy(&buf);
            if !line.contains("\"usage\"") {
                continue;
            }
            if let Some((day, project, weight)) = weigh(&line, now) {
                *self.buckets.entry((day, project)).or_insert(0.0) += weight;
            }
        }

        *cursor += consumed;
        Ok(consumed)
    }

    fn ranking(&self) -> Vec<Share> {
        let mut totals: HashMap<&str, f64> = HashMap::new();
        for ((_, project), w) in &self.buckets {
            *totals.entry(project.as_str()).or_insert(0.0) += w;
        }
        let grand: f64 = totals.values().sum();
        if grand <= 0.0 {
            return Vec::new();
        }
        let mut out: Vec<Share> = totals
            .into_iter()
            .map(|(project, w)| Share {
                project: project.to_string(),
                fraction: w / grand,
            })
            .collect();
        out.sort_by(|a, b| b.fraction.total_cmp(&a.fraction));
        out
    }
}

/// Every `.jsonl` under the projects root, including the `subagents/`
/// subdirectories sessions spawn.
fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                out.push(path);
            }
        }
    }
    out
}

/// The four fields we want out of a transcript line, and nothing else.
///
/// Deliberately a narrow struct rather than `serde_json::Value`. A single
/// assistant line can carry megabytes of `content` — full file reads, tool
/// results — and parsing into a `Value` builds an owned tree for every byte of
/// it only to drop the lot. Naming the fields lets serde walk past the big
/// ones without allocating, which is the difference between this scan costing
/// ~16 MB of retained heap and costing almost nothing.
#[derive(serde::Deserialize)]
struct Line {
    timestamp: Option<String>,
    cwd: Option<String>,
    message: Option<Message>,
}

#[derive(serde::Deserialize)]
struct Message {
    usage: Option<Usage>,
}

#[derive(serde::Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: f64,
    #[serde(default)]
    cache_creation_input_tokens: f64,
    #[serde(default)]
    cache_read_input_tokens: f64,
    #[serde(default)]
    output_tokens: f64,
}

/// `(day, project, weighted tokens)` for one transcript line, if it is an
/// assistant message inside the retained window.
fn weigh(line: &str, now: u64) -> Option<(u64, String, f64)> {
    let parsed: Line = serde_json::from_str(line).ok()?;
    let usage = parsed.message?.usage?;

    let at = crate::timefmt::parse_rfc3339(&parsed.timestamp?)?;
    let day = at / DAY;
    if day < now.saturating_sub(RETAIN_DAYS * DAY) / DAY {
        return None;
    }

    let weight = usage.input_tokens * W_INPUT
        + usage.cache_creation_input_tokens * W_CACHE_WRITE
        + usage.cache_read_input_tokens * W_CACHE_READ
        + usage.output_tokens * W_OUTPUT;
    if weight <= 0.0 {
        return None;
    }

    // `cwd` is the exact working directory; the directory name on disk is a
    // lossy encoding of the same path (separators flattened to dashes), so
    // there is no way back from it to a project name. Prefer the real thing.
    let project = parsed
        .cwd
        .as_deref()
        .map(project_name)
        .unwrap_or_else(|| "unknown".into());

    Some((day, project, weight))
}

/// Last component of a working directory: `C:\...\projects\dotnet-saas` becomes
/// `dotnet-saas`. Handles both separators, since a `cwd` written by a shell can
/// arrive either way.
fn project_name(cwd: &str) -> String {
    cwd.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or(cwd)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cwd` is given as a real path and escaped here, the way Claude Code
    /// writes it. Getting this wrong is easy and silent: an unescaped `C:\p`
    /// is not valid JSON, the line is skipped, and the project simply never
    /// shows up in the ranking.
    fn line(cwd: &str, ts: &str, out: u64) -> String {
        let cwd = cwd.replace('\\', "\\\\");
        format!(
            r#"{{"timestamp":"{ts}","cwd":"{cwd}","type":"assistant",
                "message":{{"model":"claude-opus-5","usage":{{"input_tokens":10,
                "cache_creation_input_tokens":0,"cache_read_input_tokens":0,
                "output_tokens":{out}}}}}}}"#
        )
        .replace('\n', "")
    }

    #[test]
    fn project_name_takes_the_last_component() {
        assert_eq!(project_name(r"C:\Users\jose\projects\cota"), "cota");
        assert_eq!(project_name("/home/jose/projects/cota/"), "cota");
        assert_eq!(project_name("cota"), "cota");
    }

    #[test]
    fn weighs_an_assistant_line() {
        let now = crate::timefmt::parse_rfc3339("2026-09-17T12:00:00Z").unwrap();
        let l = line(r"C:\p\alpha", "2026-09-17T09:00:00Z", 2);
        let (_, project, weight) = weigh(&l, now).expect("should weigh");
        assert_eq!(project, "alpha");
        // 10 input + 2 output * 5
        assert_eq!(weight, 20.0);
    }

    #[test]
    fn ignores_lines_outside_the_window_and_without_usage() {
        let now = crate::timefmt::parse_rfc3339("2026-09-17T12:00:00Z").unwrap();
        let old = line(r"C:\p\alpha", "2026-08-01T09:00:00Z", 2);
        assert!(weigh(&old, now).is_none());
        assert!(weigh(r#"{"type":"user","message":{"content":"hi"}}"#, now).is_none());
        assert!(weigh("not json at all", now).is_none());
    }

    #[test]
    fn ranking_is_a_normalised_share_highest_first() {
        let mut a = Attribution::default();
        a.buckets.insert((20_000, "alpha".into()), 75.0);
        a.buckets.insert((20_001, "alpha".into()), 5.0);
        a.buckets.insert((20_000, "beta".into()), 20.0);
        let r = a.ranking();
        assert_eq!(r[0].project, "alpha");
        assert!((r[0].fraction - 0.8).abs() < 1e-9);
        assert_eq!(r[1].project, "beta");
        assert!((r[1].fraction - 0.2).abs() < 1e-9);
    }

    /// A scratch file that cleans itself up.
    struct Temp(PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn temp(name: &str) -> Temp {
        Temp(std::env::temp_dir().join(format!("cota-burn-{name}.jsonl")))
    }

    fn total(a: &Attribution) -> f64 {
        a.buckets.values().sum()
    }

    /// Bare project names, no separators: path handling has its own tests, and
    /// here it is incidental detail that only makes the fixtures harder to read.
    fn record(project: &str, ts: &str) -> String {
        line(project, ts, 2)
    }

    #[test]
    fn a_line_still_being_written_is_left_for_next_time() {
        // The bug: the cursor used to advance by `line.len() + 1`, so a final
        // line with no terminator pushed it one byte past the end of the file.
        // The next scan saw `len < cursor`, assumed rotation, rewound to zero
        // and re-read everything — doubling every project's tokens. A transcript
        // is mid-write most of the time this runs.
        let now = crate::timefmt::parse_rfc3339("2026-09-17T12:00:00Z").unwrap();
        let t = temp("partial");
        let first = record("alpha", "2026-09-17T09:00:00Z");
        std::fs::write(&t.0, format!("{first}\n")).unwrap();

        let mut a = Attribution::default();
        assert_eq!(a.ingest(&t.0, now).unwrap(), first.len() as u64 + 1);
        let after_first = total(&a);
        assert!(after_first > 0.0, "the first line should have been counted");

        // Claude Code starts appending a second record and has not finished it.
        let partial = &first[..first.len() / 2];
        std::fs::write(&t.0, format!("{first}\n{partial}")).unwrap();
        assert_eq!(a.ingest(&t.0, now).unwrap(), 0, "half a line is not a line");
        assert_eq!(total(&a), after_first, "nothing new should be counted");

        // It finishes the line.
        let second = record("beta", "2026-09-17T10:00:00Z");
        std::fs::write(&t.0, format!("{first}\n{second}\n")).unwrap();
        a.ingest(&t.0, now).unwrap();
        assert_eq!(
            total(&a),
            after_first * 2.0,
            "the second line must count exactly once"
        );
        assert_eq!(a.ranking().len(), 2);
    }

    #[test]
    fn an_unchanged_file_is_not_read_again() {
        let now = crate::timefmt::parse_rfc3339("2026-09-17T12:00:00Z").unwrap();
        let t = temp("stable");
        let only = record("alpha", "2026-09-17T09:00:00Z");
        std::fs::write(&t.0, format!("{only}\n")).unwrap();

        let mut a = Attribution::default();
        a.ingest(&t.0, now).unwrap();
        let once = total(&a);
        for _ in 0..5 {
            assert_eq!(a.ingest(&t.0, now).unwrap(), 0);
        }
        assert_eq!(total(&a), once, "five idle scans must change nothing");
    }

    #[test]
    fn a_truncated_file_is_read_from_the_start_again() {
        let now = crate::timefmt::parse_rfc3339("2026-09-17T12:00:00Z").unwrap();
        let t = temp("rotated");
        let r = record("alpha", "2026-09-17T09:00:00Z");
        std::fs::write(&t.0, format!("{r}\n{r}\n")).unwrap();
        let mut a = Attribution::default();
        a.ingest(&t.0, now).unwrap();

        // Genuinely rotated: shorter than what has already been consumed.
        std::fs::write(&t.0, format!("{r}\n")).unwrap();
        assert!(
            a.ingest(&t.0, now).unwrap() > 0,
            "a real rotation must rewind"
        );
    }

    #[test]
    fn no_data_ranks_empty_rather_than_dividing_by_zero() {
        assert!(Attribution::default().ranking().is_empty());
    }
}
