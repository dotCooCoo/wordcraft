//! AutoFormat As You Type's "Format beginning of list item like the one before it": Enter at the
//! end of a list item gives the next item's lead-in, the text typed up to its first punctuation
//! character, the character formatting that the whole lead-in of the item before shares.

use std::sync::LazyLock;

use serde_json::Map;
use wordcraft_doc::props::CharProps;

use super::{CmdError, NumRef, OBJ, Pos, Session, Value, type_text_effect};
use crate::cmd::InsertEffect;

/// Unicode punctuation (general category P): where a lead-in ends.
static PUNCTUATION: LazyLock<Option<regex::Regex>> = LazyLock::new(|| regex::Regex::new(r"\p{P}").ok());

/// Most runs of the previous item's lead-in compared; a longer lead-in is not repeated.
const MAX_RUNS: usize = 256;

/// Character properties the lead-in never passes on: links, revision marks, proofing, the
/// symbol font and text direction stay as typing in the new item has them.
const KEPT: &[&str] = &["link", "ins", "del", "fmtChange", "symFont", "lang", "langBidi", "langEa", "noProof", "rtl", "hidden", "webHidden"];

/// A new list item whose lead-in is being typed with the previous item's lead-in formatting.
#[derive(Clone, Debug)]
pub(crate) struct LeadIn {
    document: u64,
    at: Pos,
    /// The formatting typing uses until the first punctuation character.
    lead: CharProps,
    /// The formatting typing would have used without the option: what follows the punctuation.
    body: CharProps,
}

/// The finished list item Enter splits, kept until the new item exists.
pub(super) struct Candidate {
    numbering: NumRef,
    runs: Vec<CharProps>,
}

fn is_punctuation(c: char) -> bool {
    let mut buf = [0u8; 4];
    PUNCTUATION.as_ref().is_some_and(|re| re.is_match(c.encode_utf8(&mut buf)))
}

/// Is the option on? Not while the document limits formatting to its styles: the lead-in's
/// formatting is direct formatting.
fn option_on(s: &Session) -> bool {
    let ac = &s.prefs.autocorrect;
    ac.enabled && ac.format_list_item_start && !(crate::cmd::protect::enforced(&s.doc) && s.doc.settings.protect_formatting)
}

/// The lead-in being typed at the caret, while typing still uses its formatting. Moving the
/// caret, picking other formatting or switching documents ends it.
fn active(s: &Session) -> Option<&LeadIn> {
    let l = s.lead_in.as_ref()?;
    let f = &s.sel.focus;
    (l.document == s.document_id() && s.sel.is_collapsed() && f.story == l.at.story && f.path == l.at.path && s.pending.as_ref() == Some(&l.lead))
        .then_some(l)
}

/// End a lead-in: typing goes on with the formatting it would have had without the option.
pub(crate) fn finish(s: &mut Session) {
    if let Some(body) = active(s).map(|l| l.body.clone()) {
        s.pending = Some(body);
    }
    s.lead_in = None;
}

/// The list item at `at` when Enter at its end starts a new item whose lead-in can follow it:
/// the option is on, the item has text and the caret is at its end. Its lead-in runs from its
/// first character that is not white space to the character before its first punctuation
/// character (all of its text when it has none); deleted and hidden text and objects are left out.
pub(super) fn candidate(s: &Session, at: &Pos) -> Option<Candidate> {
    if !option_on(s) || !s.sel.is_collapsed() {
        return None;
    }
    let para = s.doc.para_at(at)?;
    let numbering = s.doc.styles.resolve_para(&para.props).numbering.filter(|n| n.num != 0)?;
    if para.is_empty() || at.off != para.len() {
        return None;
    }
    let mut runs: Vec<CharProps> = Vec::new();
    'runs: for (range, props) in para.run_ranges() {
        if props.del.is_some() || props.hidden == Some(true) {
            continue;
        }
        let mut used = false;
        for c in para.text.get(range)?.chars() {
            if is_punctuation(c) {
                if used {
                    runs.push(props.unlinked());
                }
                break 'runs;
            }
            if c == OBJ || (c.is_whitespace() && runs.is_empty() && !used) {
                continue;
            }
            used = true;
        }
        if used {
            runs.push(props.unlinked());
            if runs.len() > MAX_RUNS {
                return None;
            }
        }
    }
    (!runs.is_empty()).then_some(Candidate { numbering, runs })
}

/// `base` with every property the runs share, set or unset alike, except [`KEPT`] ones.
fn shared(runs: &[CharProps], base: &CharProps) -> Option<CharProps> {
    let object = |c: &CharProps| match serde_json::to_value(c) {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    };
    let maps: Vec<Map<String, Value>> = runs.iter().map(object).collect::<Option<_>>()?;
    let first = maps.first()?;
    let mut out = object(base)?;
    let keys: std::collections::BTreeSet<String> = maps.iter().flat_map(|m| m.keys().cloned()).chain(out.keys().cloned()).collect();
    for k in keys.iter().filter(|k| !KEPT.contains(&k.as_str())) {
        let value = first.get(k);
        if maps.iter().all(|m| m.get(k) == value) {
            match value {
                Some(v) => out.insert(k.clone(), v.clone()),
                None => out.remove(k),
            };
        }
    }
    serde_json::from_value(Value::Object(out)).ok()
}

/// After Enter made the new item at the caret: when it is an empty item of the same list and
/// level and the lead-in's shared formatting differs from what typing would use, typing uses it.
pub(super) fn start(s: &mut Session, candidate: Option<Candidate>) {
    s.lead_in = None;
    let Some(c) = candidate else { return };
    let at = s.sel.focus.clone();
    let Some(para) = s.doc.para_at(&at) else { return };
    if !para.is_empty() || s.doc.styles.resolve_para(&para.props).numbering != Some(c.numbering) {
        return;
    }
    let body = s.typing_props();
    let Some(lead) = shared(&c.runs, &body).filter(|l| *l != body) else { return };
    s.pending = Some(lead.clone());
    s.lead_in = Some(LeadIn { document: s.document_id(), at, lead, body });
}

/// Type `text` like [`type_text_effect`]; during a lead-in, the text up to and including its first
/// punctuation character (or up to a paragraph break) takes the lead-in's formatting and the rest
/// the formatting typing would use without the option.
pub(super) fn type_text_lead(s: &mut Session, text: &str) -> Result<InsertEffect, CmdError> {
    if active(s).is_none() {
        s.lead_in = None;
        return type_text_effect(s, text);
    }
    let end = text.char_indices().find_map(|(i, c)| match c {
        '\r' | '\u{2029}' => Some(i),
        c if is_punctuation(c) => Some(i + c.len_utf8()),
        _ => None,
    });
    let Some((head, rest)) = end.and_then(|i| Some((text.get(..i)?, text.get(i..)?))) else { return type_text_effect(s, text) };
    let mut effect = if head.is_empty() { InsertEffect::default() } else { type_text_effect(s, head)? };
    finish(s);
    if !rest.is_empty() {
        let more = type_text_effect(s, rest)?;
        effect.content_changed |= more.content_changed;
        effect.state_changed |= more.state_changed;
    }
    Ok(effect)
}
