//! Provenance markers that separate managed content from local edits.
//!
//! A marked file starts with one comment line recording the house, template,
//! template revision, guidance revision, and the SHA-256 of the managed content
//! that follows the line. If the content still hashes to the recorded digest,
//! the file is unedited and a later update may replace it; otherwise the local
//! edits must be preserved.

use std::fmt::{self, Write as _};

use sha2::{Digest, Sha256};

use crate::{
    HouseId,
    contracts::CommitId,
    scaffold::{MarkerStyle, TemplateName, TemplateRevision},
};

const MARKER_KEY: &str = "kitchen-managed:";

/// Where rendered content came from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TemplateProvenance {
    /// The house that owns the template.
    pub house: HouseId,
    /// The template name.
    pub template: TemplateName,
    /// The template revision.
    pub revision: TemplateRevision,
    /// The house guidance revision the template was rendered with.
    pub guidance: CommitId,
}

/// A SHA-256 digest of managed content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentDigest([u8; 32]);

impl ContentDigest {
    /// Hash content.
    #[must_use]
    pub fn of(content: &str) -> Self {
        Self(Sha256::digest(content.as_bytes()).into())
    }

    fn parse(hex: &str) -> Option<Self> {
        if hex.len() != 64 {
            return None;
        }
        let nibble = |digit: u8| match digit {
            b'0'..=b'9' => Some(digit - b'0'),
            b'a'..=b'f' => Some(digit - b'a' + 10),
            _ => None,
        };
        let mut bytes = [0_u8; 32];
        let (pairs, _) = hex.as_bytes().as_chunks::<2>();
        for (byte, [high, low]) in bytes.iter_mut().zip(pairs) {
            *byte = nibble(*high)? << 4 | nibble(*low)?;
        }
        Some(Self(bytes))
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
            .iter()
            .try_for_each(|byte| write!(formatter, "{byte:02x}"))
    }
}

/// A parsed provenance marker.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ManagedMarker {
    /// The recorded provenance.
    pub provenance: TemplateProvenance,
    /// The digest of the managed content when it was rendered.
    pub content: ContentDigest,
}

/// Whether a file's content is managed, and whether it was edited locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedState {
    /// No valid marker: the content is local.
    Unmanaged,
    /// The content matches its marker; an update may replace it.
    Pristine(ManagedMarker),
    /// The content changed after rendering; preserve the edits.
    Edited(ManagedMarker),
}

/// Prefix `body` with a marker line in `style`.
pub(super) fn mark(style: MarkerStyle, provenance: &TemplateProvenance, body: &str) -> String {
    let digest = ContentDigest::of(body);
    let mut marked = String::with_capacity(body.len() + 256);
    let (open, close) = delimiters(style);
    // Writing to a String cannot fail.
    let _ = writeln!(
        marked,
        "{open}{MARKER_KEY} house={} template={} template-revision={} guidance-revision={} content-sha256={digest}{close}",
        provenance.house, provenance.template, provenance.revision, provenance.guidance,
    );
    marked.push_str(body);
    marked
}

const fn delimiters(style: MarkerStyle) -> (&'static str, &'static str) {
    match style {
        MarkerStyle::HtmlComment => ("<!-- ", " -->"),
        MarkerStyle::HashComment => ("# ", ""),
    }
}

/// Classify file content by its first-line provenance marker.
///
/// A malformed or partial marker reads as [`ManagedState::Unmanaged`], so its
/// content is always treated as local.
#[must_use]
pub fn inspect_managed(content: &str) -> ManagedState {
    let Some((line, body)) = content.split_once('\n') else {
        return ManagedState::Unmanaged;
    };
    let Some(marker) = parse_marker(line) else {
        return ManagedState::Unmanaged;
    };
    if ContentDigest::of(body) == marker.content {
        ManagedState::Pristine(marker)
    } else {
        ManagedState::Edited(marker)
    }
}

fn parse_marker(line: &str) -> Option<ManagedMarker> {
    let fields = [MarkerStyle::HtmlComment, MarkerStyle::HashComment]
        .into_iter()
        .find_map(|style| {
            let (open, close) = delimiters(style);
            line.strip_prefix(open)?
                .strip_suffix(close)?
                .strip_prefix(MARKER_KEY)
        })?;
    let mut values = fields.split(' ').skip_while(|field| field.is_empty());
    let mut field = |key: &str| {
        values
            .next()
            .and_then(|pair| pair.strip_prefix(key)?.strip_prefix('='))
    };
    let house = HouseId::new(field("house")?).ok()?;
    let template = TemplateName::new(field("template")?).ok()?;
    let revision = field("template-revision")?
        .parse()
        .ok()
        .map(TemplateRevision::new)?;
    let guidance = CommitId::new(field("guidance-revision")?).ok()?;
    let content = ContentDigest::parse(field("content-sha256")?)?;
    if values.next().is_some() {
        return None;
    }
    Some(ManagedMarker {
        provenance: TemplateProvenance {
            house,
            template,
            revision,
            guidance,
        },
        content,
    })
}
