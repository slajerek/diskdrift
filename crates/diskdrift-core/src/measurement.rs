//! Quality and provenance for measurements emitted by platform-independent DTOs.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Observed,
    Estimated,
    LowerBound,
    Unknown,
    Unsupported,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<Quality>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
}

impl Default for Envelope {
    fn default() -> Self {
        Self::unknown("filesystem", "record stream", "measurement unavailable")
    }
}

impl Envelope {
    pub fn with_quality(
        domain: impl Into<String>,
        scope: impl Into<String>,
        quality: Quality,
        provenance: impl Into<String>,
    ) -> Self {
        Self {
            domain: Some(domain.into()),
            scope: Some(scope.into()),
            observed_at: matches!(
                quality,
                Quality::Observed | Quality::Estimated | Quality::LowerBound
            )
            .then(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            quality: Some(quality),
            provenance: Some(provenance.into()),
        }
    }

    pub fn observed(
        domain: impl Into<String>,
        scope: impl Into<String>,
        provenance: impl Into<String>,
    ) -> Self {
        Self::with_quality(domain, scope, Quality::Observed, provenance)
    }

    pub fn lower_bound(
        domain: impl Into<String>,
        scope: impl Into<String>,
        provenance: impl Into<String>,
    ) -> Self {
        Self::with_quality(domain, scope, Quality::LowerBound, provenance)
    }

    pub fn estimated(
        domain: impl Into<String>,
        scope: impl Into<String>,
        provenance: impl Into<String>,
    ) -> Self {
        Self::with_quality(domain, scope, Quality::Estimated, provenance)
    }

    pub fn unknown(
        domain: impl Into<String>,
        scope: impl Into<String>,
        provenance: impl Into<String>,
    ) -> Self {
        Self::with_quality(domain, scope, Quality::Unknown, provenance)
    }
}
