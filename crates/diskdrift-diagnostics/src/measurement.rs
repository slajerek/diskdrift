//! Quality and provenance shared by diagnostic byte measurements.

pub use diskdrift_core::measurement::{Envelope, Quality};
use diskdrift_platform::{MeasurementQuality as PlatformQuality, VolumeCapacity};

pub fn from_capacity(capacity: &VolumeCapacity) -> Envelope {
    let quality = match capacity.quality {
        PlatformQuality::Observed => Quality::Observed,
        PlatformQuality::Estimated => Quality::Estimated,
        PlatformQuality::LowerBound => Quality::LowerBound,
        PlatformQuality::Unsupported => Quality::Unsupported,
        PlatformQuality::NotApplicable => Quality::NotApplicable,
        PlatformQuality::Unknown => Quality::Unknown,
    };
    let mut envelope = Envelope::with_quality(
        capacity
            .domain
            .clone()
            .unwrap_or_else(|| "filesystem".into()),
        capacity.scope.clone().unwrap_or_else(|| "volume".into()),
        quality,
        capacity
            .provenance
            .clone()
            .unwrap_or_else(|| "platform volume adapter".into()),
    );
    if capacity.observed_at.is_some() {
        envelope.observed_at = capacity.observed_at.clone();
    }
    envelope.domain = capacity.domain.clone().or(envelope.domain);
    envelope.scope = capacity.scope.clone().or(envelope.scope);
    envelope.provenance = capacity.provenance.clone().or(envelope.provenance);
    envelope
}
