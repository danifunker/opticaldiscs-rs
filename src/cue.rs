//! CUE sheet parsing.
//!
//! A CUE sheet is line-oriented: one command per line, a keyword followed by its
//! arguments, with double quotes around anything that can contain spaces. This
//! parser deliberately accepts a wider grammar than the CDRWIN reference, because
//! the sheets that ship on real media are wider than it:
//!
//! - **Numbers need not be zero-padded.** Every example in the spec shows
//!   `TRACK 01` / `INDEX 01`, but plenty of tools write `TRACK 1` — including
//!   whoever mastered Microsoft Bookshelf, so this is not a hand-written-file
//!   edge case.
//! - **MSF fields need not be zero-padded either**, and minutes may run past two
//!   digits on an over-long image.
//! - **A `REM` value runs to the end of the line.** `REM GENRE Alternative Rock`
//!   is one remark, not a remark followed by a stray `Rock` command.
//! - **Unrecognised keywords are skipped rather than fatal.** Vendors invent
//!   their own, and none of them change the track layout.
//!
//! Only syntax lives here. Turning a `TRACK` format string into a
//! [`TrackType`](crate::bincue::TrackType) is [`crate::bincue`]'s job.

use std::str::FromStr;

use crate::error::{OpticaldiscsError, Result};

// ── Time ──────────────────────────────────────────────────────────────────────

/// An `mm:ss:ff` CUE timestamp. There are 75 frames (sectors) per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CueTime {
    /// Minutes. Not capped at 99 — a few writers emit longer addresses.
    pub minutes: u32,
    /// Seconds within the minute.
    pub seconds: u32,
    /// Frames within the second (75 per second).
    pub frames: u32,
}

impl CueTime {
    /// Total frame (sector) count this timestamp addresses.
    pub fn to_frames(self) -> u64 {
        self.minutes as u64 * 60 * 75 + self.seconds as u64 * 75 + self.frames as u64
    }
}

impl FromStr for CueTime {
    type Err = String;

    /// Parse `m:s:f`, each field one or more digits.
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let mut parts = s.split(':');
        let (Some(m), Some(sec), Some(f), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(format!("expected mm:ss:ff, found {s:?}"));
        };
        let field = |name: &str, v: &str| -> std::result::Result<u32, String> {
            v.parse::<u32>()
                .map_err(|_| format!("{name} of {s:?} is not a number"))
        };
        Ok(CueTime {
            minutes: field("minutes", m)?,
            seconds: field("seconds", sec)?,
            frames: field("frames", f)?,
        })
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

/// One parsed line of a CUE sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CueCommand {
    /// `CATALOG` — media catalogue number (UPC/EAN), kept as written.
    Catalog(String),
    /// `CDTEXTFILE` — path to a binary CD-Text file.
    CdTextFile(String),
    /// `FILE` — the data file that subsequent tracks live in.
    File {
        /// Filename exactly as written in the sheet, quotes stripped.
        name: String,
        /// Storage format token (`BINARY`, `WAVE`, `MOTOROLA`, …), uppercased.
        format: String,
    },
    /// `FLAGS` — subcode flags for the current track (`DCP`, `4CH`, `PRE`, `SCMS`).
    Flags(Vec<String>),
    /// `INDEX` — an index point within the current track.
    Index {
        /// Index number: `0` is the pregap, `1` the track proper.
        number: u32,
        /// Position relative to the start of the current `FILE`.
        time: CueTime,
    },
    /// `ISRC` — recording code for the current track.
    Isrc(String),
    /// `PERFORMER` — CD-Text performer, for the disc or the current track.
    Performer(String),
    /// `POSTGAP` — silence appended after the current track.
    Postgap(CueTime),
    /// `PREGAP` — silence inserted before the current track. Unlike an
    /// `INDEX 00`, this gap is *not* present in the data file.
    Pregap(CueTime),
    /// `REM` — a remark. The value is the rest of the line.
    Rem {
        /// First word after `REM`, uppercased (`GENRE`, `DATE`, `DISCID`, …).
        key: String,
        /// Everything after the key, verbatim minus surrounding quotes.
        value: String,
    },
    /// `SONGWRITER` — CD-Text songwriter, for the disc or the current track.
    Songwriter(String),
    /// `TITLE` — CD-Text title, for the disc or the current track.
    Title(String),
    /// `TRACK` — starts a new track.
    Track {
        /// 1-based track number.
        number: u32,
        /// Format token (`AUDIO`, `MODE1/2352`, …), uppercased.
        format: String,
    },
    /// A keyword this parser does not model, kept verbatim so that a vendor
    /// extension never fails the sheet.
    Other {
        /// The keyword, uppercased.
        keyword: String,
        /// Its arguments, in order.
        args: Vec<String>,
    },
}

// ── Parser ────────────────────────────────────────────────────────────────────

/// Parse a CUE sheet into its commands, in file order.
///
/// # Errors
///
/// Returns [`OpticaldiscsError::Cue`] only for lines whose meaning cannot be
/// recovered — an unterminated quote, or a `TRACK`/`INDEX`/`PREGAP`/`POSTGAP`
/// whose number or timestamp is missing or unparseable. Every such message names
/// the offending line number. Unknown keywords are not errors.
pub fn parse_cue(source: &str) -> Result<Vec<CueCommand>> {
    let mut commands = Vec::new();

    for (i, raw_line) in source.lines().enumerate() {
        let lineno = i + 1;
        // A UTF-8 BOM only ever leads the file, but stripping it per line costs
        // nothing and keeps the first-line case from needing its own branch.
        let line = raw_line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() {
            continue;
        }

        let (head, rest) = match line.split_once(char::is_whitespace) {
            Some((h, r)) => (h, r.trim()),
            None => (line, ""),
        };
        let keyword = head.to_ascii_uppercase();

        // REM is the one command whose argument is free text, so it is handled
        // before tokenization: splitting it into tokens would turn the tail of
        // `REM GENRE Alternative Rock` into a command of its own.
        if keyword == "REM" {
            let (key, value) = match rest.split_once(char::is_whitespace) {
                Some((k, v)) => (k, v.trim()),
                None => (rest, ""),
            };
            commands.push(CueCommand::Rem {
                key: key.to_ascii_uppercase(),
                value: unquote(value).to_string(),
            });
            continue;
        }

        let args = tokenize(rest).map_err(|e| cue_err(lineno, line, &e))?;
        commands.push(parse_command(&keyword, args).map_err(|e| cue_err(lineno, line, &e))?);
    }

    Ok(commands)
}

fn cue_err(lineno: usize, line: &str, msg: &str) -> OpticaldiscsError {
    OpticaldiscsError::Cue(format!("line {lineno}: {msg} (in {line:?})"))
}

/// Build a command from its keyword and already-tokenized arguments.
fn parse_command(keyword: &str, mut args: Vec<String>) -> std::result::Result<CueCommand, String> {
    /// Take argument `n`, or report what was expected there.
    fn arg(args: &mut [String], n: usize, what: &str) -> std::result::Result<String, String> {
        if n < args.len() {
            Ok(std::mem::take(&mut args[n]))
        } else {
            Err(format!("missing {what}"))
        }
    }

    fn number(s: &str, what: &str) -> std::result::Result<u32, String> {
        s.parse::<u32>()
            .map_err(|_| format!("{what} is not a number: {s:?}"))
    }

    fn time(s: &str, what: &str) -> std::result::Result<CueTime, String> {
        s.parse::<CueTime>().map_err(|e| format!("{what}: {e}"))
    }

    Ok(match keyword {
        "CATALOG" => CueCommand::Catalog(arg(&mut args, 0, "CATALOG number")?),
        "CDTEXTFILE" => CueCommand::CdTextFile(arg(&mut args, 0, "CDTEXTFILE path")?),
        "FILE" => {
            let name = arg(&mut args, 0, "FILE name")?;
            // The format token is optional in practice; BINARY is the only
            // sensible default for a sheet that omits it.
            let format = args
                .get(1)
                .map(|f| f.to_ascii_uppercase())
                .unwrap_or_else(|| "BINARY".to_string());
            CueCommand::File { name, format }
        }
        "FLAGS" => CueCommand::Flags(args.iter().map(|f| f.to_ascii_uppercase()).collect()),
        "INDEX" => CueCommand::Index {
            number: number(&arg(&mut args, 0, "INDEX number")?, "INDEX number")?,
            time: time(&arg(&mut args, 1, "INDEX position")?, "INDEX position")?,
        },
        "ISRC" => CueCommand::Isrc(arg(&mut args, 0, "ISRC code")?),
        "PERFORMER" => CueCommand::Performer(arg(&mut args, 0, "PERFORMER name")?),
        "POSTGAP" => CueCommand::Postgap(time(&arg(&mut args, 0, "POSTGAP length")?, "POSTGAP")?),
        "PREGAP" => CueCommand::Pregap(time(&arg(&mut args, 0, "PREGAP length")?, "PREGAP")?),
        "SONGWRITER" => CueCommand::Songwriter(arg(&mut args, 0, "SONGWRITER name")?),
        "TITLE" => CueCommand::Title(arg(&mut args, 0, "TITLE text")?),
        "TRACK" => CueCommand::Track {
            number: number(&arg(&mut args, 0, "TRACK number")?, "TRACK number")?,
            format: arg(&mut args, 1, "TRACK format")?.to_ascii_uppercase(),
        },
        other => CueCommand::Other {
            keyword: other.to_string(),
            args,
        },
    })
}

/// Split a line's arguments into tokens, treating a double-quoted run as one
/// token and passing its contents through byte for byte.
fn tokenize(s: &str) -> std::result::Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut chars = s.chars().peekable();

    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' {
            chars.next();
            let mut token = String::new();
            let mut closed = false;
            for ch in chars.by_ref() {
                if ch == '"' {
                    closed = true;
                    break;
                }
                token.push(ch);
            }
            if !closed {
                return Err("unterminated quoted string".to_string());
            }
            tokens.push(token);
        } else {
            let mut token = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace() {
                    break;
                }
                token.push(ch);
                chars.next();
            }
            tokens.push(token);
        }
    }

    Ok(tokens)
}

/// Strip one layer of surrounding double quotes, if present.
fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn only(source: &str) -> CueCommand {
        let mut c = parse_cue(source).unwrap();
        assert_eq!(c.len(), 1, "expected one command from {source:?}");
        c.remove(0)
    }

    #[test]
    fn time_accepts_padded_and_unpadded() {
        assert_eq!(
            "01:23:45".parse::<CueTime>().unwrap(),
            CueTime {
                minutes: 1,
                seconds: 23,
                frames: 45
            }
        );
        assert_eq!(
            "1:2:3".parse::<CueTime>().unwrap(),
            CueTime {
                minutes: 1,
                seconds: 2,
                frames: 3
            }
        );
        // Minutes past 99 are not rejected — a few writers emit them.
        assert_eq!("120:00:00".parse::<CueTime>().unwrap().minutes, 120);
    }

    #[test]
    fn time_rejects_malformed() {
        assert!("01:23".parse::<CueTime>().is_err());
        assert!("01:23:45:67".parse::<CueTime>().is_err());
        assert!("aa:bb:cc".parse::<CueTime>().is_err());
    }

    #[test]
    fn time_to_frames() {
        assert_eq!("00:00:00".parse::<CueTime>().unwrap().to_frames(), 0);
        assert_eq!(
            "01:23:45".parse::<CueTime>().unwrap().to_frames(),
            60 * 75 + 23 * 75 + 45
        );
    }

    #[test]
    fn unpadded_track_and_index() {
        assert_eq!(
            only("TRACK 1 MODE1/2352"),
            CueCommand::Track {
                number: 1,
                format: "MODE1/2352".into()
            }
        );
        assert_eq!(
            only("INDEX 1 00:00:00"),
            CueCommand::Index {
                number: 1,
                time: CueTime::default()
            }
        );
    }

    #[test]
    fn padded_and_unpadded_agree() {
        let padded = parse_cue("TRACK 01 AUDIO\nINDEX 01 00:02:00\n").unwrap();
        let unpadded = parse_cue("TRACK 1 AUDIO\nINDEX 1 0:2:0\n").unwrap();
        assert_eq!(padded, unpadded);
    }

    #[test]
    fn file_keeps_name_byte_exact() {
        // The name must survive verbatim: an earlier implementation rewrote
        // keyword-looking substrings across the whole sheet and corrupted names
        // like this one on case-sensitive filesystems.
        assert_eq!(
            only("FILE \"MY BINARY DISC.bin\" BINARY"),
            CueCommand::File {
                name: "MY BINARY DISC.bin".into(),
                format: "BINARY".into()
            }
        );
    }

    #[test]
    fn file_format_defaults_to_binary() {
        assert_eq!(
            only("FILE \"disc.bin\""),
            CueCommand::File {
                name: "disc.bin".into(),
                format: "BINARY".into()
            }
        );
    }

    #[test]
    fn file_accepts_unquoted_name() {
        assert_eq!(
            only("FILE disc.bin BINARY"),
            CueCommand::File {
                name: "disc.bin".into(),
                format: "BINARY".into()
            }
        );
    }

    #[test]
    fn rem_value_runs_to_end_of_line() {
        assert_eq!(
            only("REM GENRE Alternative Rock"),
            CueCommand::Rem {
                key: "GENRE".into(),
                value: "Alternative Rock".into()
            }
        );
        assert_eq!(
            only("REM COMMENT \"ExactAudioCopy v0.99pb4\""),
            CueCommand::Rem {
                key: "COMMENT".into(),
                value: "ExactAudioCopy v0.99pb4".into()
            }
        );
        assert_eq!(
            only("REM"),
            CueCommand::Rem {
                key: String::new(),
                value: String::new()
            }
        );
    }

    #[test]
    fn catalog_is_kept_not_dropped() {
        // 13 digits overflow nothing here; the previous parser needed these
        // lines stripped before it would accept the sheet at all.
        assert_eq!(
            only("CATALOG 0000000000000"),
            CueCommand::Catalog("0000000000000".into())
        );
    }

    #[test]
    fn keywords_are_case_insensitive() {
        assert_eq!(
            only("track 01 audio"),
            CueCommand::Track {
                number: 1,
                format: "AUDIO".into()
            }
        );
    }

    #[test]
    fn unknown_keyword_is_not_fatal() {
        assert_eq!(
            only("VENDORTHING 42 abc"),
            CueCommand::Other {
                keyword: "VENDORTHING".into(),
                args: vec!["42".into(), "abc".into()]
            }
        );
    }

    #[test]
    fn crlf_and_blank_lines() {
        let cmds = parse_cue("\r\nFILE \"a.bin\" BINARY\r\n\r\n  TRACK 01 AUDIO\r\n").unwrap();
        assert_eq!(cmds.len(), 2);
    }

    #[test]
    fn leading_bom_is_ignored() {
        let cmds = parse_cue("\u{feff}FILE \"a.bin\" BINARY\n").unwrap();
        assert_eq!(cmds.len(), 1);
    }

    #[test]
    fn errors_name_the_line() {
        let err = parse_cue("FILE \"a.bin\" BINARY\nTRACK XX AUDIO\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("line 2"), "{msg}");
        assert!(msg.contains("TRACK number"), "{msg}");
    }

    #[test]
    fn unterminated_quote_is_an_error() {
        assert!(parse_cue("FILE \"a.bin BINARY\n").is_err());
    }

    #[test]
    fn missing_index_time_is_an_error() {
        assert!(parse_cue("INDEX 01\n").is_err());
    }

    #[test]
    fn full_eac_style_sheet() {
        let sheet = "REM GENRE Alternative Rock\n\
                     REM DATE 1997\n\
                     REM DISCID 8A0B6A0B\n\
                     REM COMMENT \"ExactAudioCopy v0.99pb4\"\n\
                     PERFORMER \"An Artist\"\n\
                     TITLE \"An Album\"\n\
                     FILE \"An Album.wav\" WAVE\n\
                     \x20 TRACK 01 AUDIO\n\
                     \x20   TITLE \"First Song\"\n\
                     \x20   PERFORMER \"An Artist\"\n\
                     \x20   ISRC ABCDE1234567\n\
                     \x20   FLAGS DCP\n\
                     \x20   PREGAP 00:02:00\n\
                     \x20   INDEX 01 00:00:00\n";
        let cmds = parse_cue(sheet).unwrap();
        assert_eq!(cmds.len(), 14);
        assert!(matches!(cmds[6], CueCommand::File { .. }));
        assert!(matches!(cmds[7], CueCommand::Track { number: 1, .. }));
        assert_eq!(cmds[11], CueCommand::Flags(vec!["DCP".into()]));
    }
}
