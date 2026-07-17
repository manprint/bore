#![allow(missing_docs)]

use crate::shared::{
    UdpAdaptiveCandidateKind, UdpAdaptiveMode, UdpAdaptivePlan, UdpCandidateKind,
    UdpCandidateOffer, UdpNatFiltering, UdpNatMapping, UdpNatProfile, UdpTestPeerSummary,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NatMappingClass {
    Open,
    Cone,
    Symmetric,
    Blocked,
    Inconclusive,
}

impl NatMappingClass {
    fn from_label(label: &str) -> Self {
        let normalized = label.trim().to_ascii_lowercase();
        if normalized.contains("blocked") {
            Self::Blocked
        } else if normalized.contains("inconclusive") {
            Self::Inconclusive
        } else if normalized.contains("symmetric") {
            Self::Symmetric
        } else if normalized.contains("cone") {
            Self::Cone
        } else if normalized.contains("open") {
            Self::Open
        } else {
            Self::Inconclusive
        }
    }

    fn direct_friendly(self) -> bool {
        matches!(self, Self::Open | Self::Cone)
    }

    fn symmetric(self) -> bool {
        matches!(self, Self::Symmetric)
    }

    fn blocked(self) -> bool {
        matches!(self, Self::Blocked)
    }

    fn inconclusive(self) -> bool {
        matches!(self, Self::Inconclusive)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NatCandidateKind {
    Reflexive,
    Local,
    RouterMapped,
    Predicted,
    RelayFallback,
}

impl NatCandidateKind {
    fn from_udp_kind(kind: UdpCandidateKind) -> Self {
        match kind {
            UdpCandidateKind::Reflexive => Self::Reflexive,
            UdpCandidateKind::RouterMapped => Self::RouterMapped,
            UdpCandidateKind::Predicted => Self::Predicted,
            UdpCandidateKind::Local => Self::Local,
        }
    }

    fn to_wire(self) -> UdpAdaptiveCandidateKind {
        match self {
            Self::Reflexive => UdpAdaptiveCandidateKind::Reflexive,
            Self::Local => UdpAdaptiveCandidateKind::Local,
            Self::RouterMapped => UdpAdaptiveCandidateKind::RouterMapped,
            Self::Predicted => UdpAdaptiveCandidateKind::Predicted,
            Self::RelayFallback => UdpAdaptiveCandidateKind::RelayFallback,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Reflexive => "reflexive",
            Self::Local => "local",
            Self::RouterMapped => "router-mapped",
            Self::Predicted => "predicted",
            Self::RelayFallback => "relay",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NatPlanMode {
    DirectFirst,
    DirectWithRetry,
    RelayFirst,
    RelayOnly,
}

impl NatPlanMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::DirectFirst => "direct-first",
            Self::DirectWithRetry => "direct-with-retry",
            Self::RelayFirst => "relay-first",
            Self::RelayOnly => "relay-only",
        }
    }

    fn to_wire(self) -> UdpAdaptiveMode {
        match self {
            Self::DirectFirst => UdpAdaptiveMode::DirectFirst,
            Self::DirectWithRetry => UdpAdaptiveMode::DirectWithRetry,
            Self::RelayFirst => UdpAdaptiveMode::RelayFirst,
            Self::RelayOnly => UdpAdaptiveMode::RelayOnly,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NatProfile {
    pub(crate) mapping_class: NatMappingClass,
    /// Observed filtering behaviour (structured wire profile only; always
    /// `Unknown` from a summary — a live gather cannot observe filtering
    /// until Fase 6's two-IP STUN server).
    pub(crate) filtering: UdpNatFiltering,
    /// Independent STUN observations backing the mapping class (confidence).
    pub(crate) observations: u8,
    pub(crate) local_udp: String,
    pub(crate) selected_stun: Option<String>,
    pub(crate) candidate_kinds: Vec<NatCandidateKind>,
    pub(crate) candidate_count: usize,
    pub(crate) reflexive_count: usize,
    pub(crate) port_preserved: Option<bool>,
}

impl NatProfile {
    /// Legacy constructor for the paired `bore test-udp` diagnostic, whose
    /// summaries carry a human label. The ONLY place label parsing is still
    /// allowed; live tunnels use [`Self::from_wire`] (structured, plan Fase 3).
    pub(crate) fn from_summary(summary: &UdpTestPeerSummary) -> Self {
        let mapping_class = NatMappingClass::from_label(&summary.nat_class);
        Self {
            mapping_class,
            filtering: UdpNatFiltering::Unknown,
            observations: u8::from(summary.selected_stun.is_some()),
            local_udp: summary.local_udp.clone(),
            selected_stun: summary.selected_stun.clone(),
            candidate_kinds: summary
                .candidate_kinds
                .iter()
                .copied()
                .map(NatCandidateKind::from_udp_kind)
                .collect(),
            candidate_count: summary.candidate_count,
            reflexive_count: summary.reflexive.len(),
            port_preserved: summary.port_preserved,
        }
    }

    /// Structured constructor from a live offer's wire profile (plan Fase 3):
    /// no text parsing anywhere. `Unknown` mapping with zero observations AND
    /// no reachable-looking candidate (reflexive OR router-mapped) means STUN
    /// itself failed (blocked-ish); a router-mapped candidate — an explicit
    /// port mapping or an operator-declared `--udp-candidate` endpoint (plan
    /// Fase 5, possibly with `--udp-no-stun`) — keeps the peer off the
    /// blocked classification even with zero STUN observations. `Unknown`
    /// with observations means "single observation, cannot classify"
    /// (inconclusive — never punished as hostile).
    pub(crate) fn from_wire(profile: &UdpNatProfile, offer: &UdpCandidateOffer) -> Self {
        let reflexive_count = offer
            .typed_candidates
            .iter()
            .filter(|c| c.kind == UdpCandidateKind::Reflexive)
            .count();
        let router_mapped = offer
            .typed_candidates
            .iter()
            .any(|c| c.kind == UdpCandidateKind::RouterMapped);
        let mapping_class = match profile.mapping {
            UdpNatMapping::Eim => NatMappingClass::Cone,
            UdpNatMapping::Symmetric => NatMappingClass::Symmetric,
            UdpNatMapping::Unknown
                if profile.observations == 0 && reflexive_count == 0 && !router_mapped =>
            {
                NatMappingClass::Blocked
            }
            UdpNatMapping::Unknown => NatMappingClass::Inconclusive,
        };
        Self {
            mapping_class,
            filtering: profile.filtering,
            observations: profile.observations,
            local_udp: String::new(),
            selected_stun: offer.selected_stun.clone(),
            candidate_kinds: offer
                .typed_candidates
                .iter()
                .map(|c| NatCandidateKind::from_udp_kind(c.kind))
                .collect(),
            candidate_count: offer.candidates.len(),
            reflexive_count,
            port_preserved: profile.port_preserved,
        }
    }

    fn has_candidate_kind(&self, kind: NatCandidateKind) -> bool {
        self.candidate_kinds.contains(&kind)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NatPlan {
    pub(crate) mode: NatPlanMode,
    pub(crate) candidate_order: Vec<NatCandidateKind>,
    pub(crate) retry_budget: u8,
    pub(crate) read_timeout_ms: u64,
    pub(crate) send_delay_ms: u64,
    /// Stable machine-parseable code for WHY this mode was chosen (logged
    /// alongside the human `reasons`; documented in docs/nat/NAT_TRAVERSAL.md).
    pub(crate) reason_code: &'static str,
    pub(crate) reasons: Vec<String>,
}

impl NatPlan {
    pub(crate) fn summary(&self) -> String {
        let order = self
            .candidate_order
            .iter()
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>()
            .join(" -> ");
        format!(
            "{} (retry {}, read {}ms, delay {}ms, order {})",
            self.mode.as_str(),
            self.retry_budget,
            self.read_timeout_ms,
            self.send_delay_ms,
            order
        )
    }

    /// Summary plus the stable reason code (server-side broker logs).
    pub(crate) fn summary_with_reason(&self) -> String {
        format!("{} [reason={}]", self.summary(), self.reason_code)
    }

    pub(crate) fn to_wire(&self) -> UdpAdaptivePlan {
        UdpAdaptivePlan {
            mode: self.mode.to_wire(),
            candidate_order: self
                .candidate_order
                .iter()
                .copied()
                .map(NatCandidateKind::to_wire)
                .collect(),
            retry_budget: self.retry_budget,
            read_timeout_ms: self.read_timeout_ms,
            send_delay_ms: self.send_delay_ms,
        }
    }
}

pub(crate) fn plan_for_pair(local: &NatProfile, peer: &NatProfile) -> NatPlan {
    let (mode, reason_code, reason) = select_mode(local, peer);
    let candidate_order = candidate_order(local, peer, mode);
    let (retry_budget, read_timeout_ms, send_delay_ms) = match mode {
        NatPlanMode::DirectFirst => (1, 750, 0),
        NatPlanMode::DirectWithRetry => (2, 500, 25),
        NatPlanMode::RelayFirst => (1, 250, 0),
        NatPlanMode::RelayOnly => (0, 0, 0),
    };

    NatPlan {
        mode,
        candidate_order,
        retry_budget,
        read_timeout_ms,
        send_delay_ms,
        reason_code,
        reasons: vec![reason],
    }
}

/// Mode decision over the structured pair: mapping first, then filtering,
/// then confidence. Every arm returns a stable reason code (documented in
/// docs/nat/NAT_TRAVERSAL.md §17) plus the human sentence. Absent/partial
/// metadata degrades toward DirectWithRetry, NEVER RelayOnly (a missing
/// profile is not a hostile NAT).
fn select_mode(local: &NatProfile, peer: &NatProfile) -> (NatPlanMode, &'static str, String) {
    if local.candidate_count == 0 && peer.candidate_count == 0 {
        return (
            NatPlanMode::RelayOnly,
            "no-candidates",
            "no usable direct candidates were reported by either peer".to_string(),
        );
    }

    if local.mapping_class.direct_friendly() && peer.mapping_class.direct_friendly() {
        return (
            NatPlanMode::DirectFirst,
            "both-direct-friendly",
            "both peers look endpoint-independent or public".to_string(),
        );
    }

    if local.mapping_class.blocked() || peer.mapping_class.blocked() {
        return (
            NatPlanMode::RelayFirst,
            "peer-blocked",
            "one peer reported no STUN reachability, so relay stays first-choice".to_string(),
        );
    }

    if local.mapping_class.symmetric() || peer.mapping_class.symmetric() {
        // Symmetric mapping + address/port-dependent filtering on the SAME
        // side is the worst RFC 4787 combination (port prediction rarely
        // lands): don't burn the retry budget on it.
        let apdm_apdf = (local.mapping_class.symmetric()
            && local.filtering == UdpNatFiltering::AddressDependent)
            || (peer.mapping_class.symmetric()
                && peer.filtering == UdpNatFiltering::AddressDependent);
        if apdm_apdf {
            return (
                NatPlanMode::RelayFirst,
                "symmetric-strict-filtering",
                "a symmetric peer also filters per address/port; relay first".to_string(),
            );
        }
        if local.port_preserved == Some(true)
            || peer.port_preserved == Some(true)
            || local.selected_stun.is_some()
            || peer.selected_stun.is_some()
        {
            return (
                NatPlanMode::DirectWithRetry,
                "symmetric-escape",
                "direct path looks plausible but one peer needs extra retry room".to_string(),
            );
        }
        return (
            NatPlanMode::RelayFirst,
            "symmetric-relay",
            "a symmetric peer with no escape hatch keeps relay first-choice".to_string(),
        );
    }

    if local.mapping_class.inconclusive() || peer.mapping_class.inconclusive() {
        return (
            NatPlanMode::DirectWithRetry,
            "inconclusive",
            "mapping could not be classified with confidence; direct with retry".to_string(),
        );
    }

    (
        NatPlanMode::DirectWithRetry,
        "default",
        "no rule matched; conservative direct with retry".to_string(),
    )
}

fn candidate_order(
    local: &NatProfile,
    peer: &NatProfile,
    mode: NatPlanMode,
) -> Vec<NatCandidateKind> {
    let mut direct = Vec::new();
    if !local.candidate_kinds.is_empty() || !peer.candidate_kinds.is_empty() {
        // Same-LAN local first (free, instant win or instant fail), then the
        // cross-NAT workhorse, router/manual mappings, predicted last —
        // aligned with `holepunch::candidate_priority` and the Fase-0 lab
        // data (pair-priority order, plan Fase 3).
        for kind in [
            NatCandidateKind::Local,
            NatCandidateKind::Reflexive,
            NatCandidateKind::RouterMapped,
            NatCandidateKind::Predicted,
        ] {
            if local.has_candidate_kind(kind) || peer.has_candidate_kind(kind) {
                direct.push(kind);
            }
        }
    } else {
        if !local.local_udp.is_empty() || !peer.local_udp.is_empty() {
            direct.push(NatCandidateKind::Local);
        }
        if local.reflexive_count > 0 || peer.reflexive_count > 0 {
            direct.push(NatCandidateKind::Reflexive);
        }
        if matches!(
            local.mapping_class,
            NatMappingClass::Symmetric | NatMappingClass::Blocked | NatMappingClass::Inconclusive
        ) || matches!(
            peer.mapping_class,
            NatMappingClass::Symmetric | NatMappingClass::Blocked | NatMappingClass::Inconclusive
        ) || local.port_preserved == Some(false)
            || peer.port_preserved == Some(false)
        {
            direct.push(NatCandidateKind::Predicted);
        }
    }
    direct.push(NatCandidateKind::RelayFallback);
    direct.dedup();

    match mode {
        NatPlanMode::DirectFirst | NatPlanMode::DirectWithRetry => direct,
        NatPlanMode::RelayFirst => {
            let mut order = vec![NatCandidateKind::RelayFallback];
            order.extend(
                direct
                    .into_iter()
                    .filter(|kind| *kind != NatCandidateKind::RelayFallback),
            );
            order
        }
        NatPlanMode::RelayOnly => vec![NatCandidateKind::RelayFallback],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(
        nat_class: &str,
        selected_stun: Option<&str>,
        candidate_count: usize,
        port_preserved: Option<bool>,
        candidate_kinds: &[UdpCandidateKind],
        reflexive: &[&str],
    ) -> UdpTestPeerSummary {
        UdpTestPeerSummary {
            nat_class: nat_class.to_string(),
            local_udp: "127.0.0.1:50000".to_string(),
            primary_local_ip: Some("127.0.0.1".to_string()),
            reflexive: reflexive.iter().map(|value| value.to_string()).collect(),
            candidate_kinds: candidate_kinds.to_vec(),
            selected_stun: selected_stun.map(str::to_string),
            bore_stun: Some(true),
            candidate_count,
            port_preserved,
        }
    }

    #[test]
    fn profile_tracks_selected_stun_and_counts() {
        let profile = NatProfile::from_summary(&summary(
            "cone",
            Some("stun.cloudflare.com:3478"),
            3,
            Some(true),
            &[
                UdpCandidateKind::Reflexive,
                UdpCandidateKind::RouterMapped,
                UdpCandidateKind::Local,
            ],
            &["198.51.100.10:50000"],
        ));

        assert_eq!(profile.mapping_class, NatMappingClass::Cone);
        assert_eq!(
            profile.candidate_kinds,
            vec![
                NatCandidateKind::Reflexive,
                NatCandidateKind::RouterMapped,
                NatCandidateKind::Local,
            ]
        );
        assert_eq!(
            profile.selected_stun.as_deref(),
            Some("stun.cloudflare.com:3478")
        );
        assert_eq!(profile.candidate_count, 3);
        assert_eq!(profile.reflexive_count, 1);
        assert_eq!(profile.port_preserved, Some(true));
    }

    #[test]
    fn plan_prefers_direct_for_cone_pairs() {
        let local = NatProfile::from_summary(&summary(
            "cone",
            Some("stun.cloudflare.com:3478"),
            2,
            Some(true),
            &[
                UdpCandidateKind::Reflexive,
                UdpCandidateKind::RouterMapped,
                UdpCandidateKind::Local,
            ],
            &["198.51.100.10:50000"],
        ));
        let peer = NatProfile::from_summary(&summary(
            "open/public",
            Some("stun.cloudflare.com:3478"),
            2,
            Some(true),
            &[UdpCandidateKind::Reflexive, UdpCandidateKind::Local],
            &["198.51.100.11:50001"],
        ));

        let plan = plan_for_pair(&local, &peer);

        assert_eq!(plan.mode, NatPlanMode::DirectFirst);
        assert_eq!(
            plan.candidate_order,
            vec![
                NatCandidateKind::Local,
                NatCandidateKind::Reflexive,
                NatCandidateKind::RouterMapped,
                NatCandidateKind::RelayFallback,
            ]
        );
        assert_eq!(plan.retry_budget, 1);
        assert_eq!(plan.reason_code, "both-direct-friendly");
    }

    #[test]
    fn plan_retries_direct_when_symmetric_but_port_preserved() {
        let local = NatProfile::from_summary(&summary(
            "symmetric-random",
            Some("stun.cloudflare.com:3478"),
            4,
            Some(true),
            &[
                UdpCandidateKind::Reflexive,
                UdpCandidateKind::Predicted,
                UdpCandidateKind::Local,
            ],
            &["198.51.100.10:50000"],
        ));
        let peer = NatProfile::from_summary(&summary(
            "cone",
            Some("stun.cloudflare.com:3478"),
            2,
            Some(true),
            &[UdpCandidateKind::Reflexive, UdpCandidateKind::Local],
            &["198.51.100.11:50001"],
        ));

        let plan = plan_for_pair(&local, &peer);

        assert_eq!(plan.mode, NatPlanMode::DirectWithRetry);
        assert_eq!(plan.retry_budget, 2);
        assert!(plan.candidate_order.contains(&NatCandidateKind::Predicted));
        assert_eq!(
            plan.candidate_order.last(),
            Some(&NatCandidateKind::RelayFallback)
        );
    }

    #[test]
    fn plan_falls_back_to_relay_first_for_blocked_peer() {
        let local = NatProfile::from_summary(&summary(
            "blocked",
            None,
            1,
            None,
            &[UdpCandidateKind::Reflexive, UdpCandidateKind::Local],
            &["198.51.100.10:50000"],
        ));
        let peer = NatProfile::from_summary(&summary(
            "cone",
            None,
            2,
            Some(true),
            &[UdpCandidateKind::Reflexive, UdpCandidateKind::Local],
            &["198.51.100.11:50001"],
        ));

        let plan = plan_for_pair(&local, &peer);

        assert_eq!(plan.mode, NatPlanMode::RelayFirst);
        assert_eq!(
            plan.candidate_order.first(),
            Some(&NatCandidateKind::RelayFallback)
        );
        assert!(plan.reasons[0].contains("relay"));
    }

    #[test]
    fn plan_summary_includes_candidate_order() {
        let local = NatProfile::from_summary(&summary(
            "cone",
            Some("stun.cloudflare.com:3478"),
            3,
            Some(true),
            &[
                UdpCandidateKind::Reflexive,
                UdpCandidateKind::RouterMapped,
                UdpCandidateKind::Local,
            ],
            &["198.51.100.10:50000"],
        ));
        let peer = NatProfile::from_summary(&summary(
            "cone",
            Some("stun.cloudflare.com:3478"),
            2,
            Some(true),
            &[UdpCandidateKind::Reflexive, UdpCandidateKind::Local],
            &["198.51.100.11:50001"],
        ));

        let plan = plan_for_pair(&local, &peer);
        let summary = plan.summary();

        assert!(summary.contains("direct-first"));
        assert!(summary.contains("local -> reflexive -> router-mapped"));
    }

    #[test]
    fn plan_to_wire_preserves_mode_and_order() {
        let local = NatProfile::from_summary(&summary(
            "cone",
            Some("stun.cloudflare.com:3478"),
            3,
            Some(true),
            &[
                UdpCandidateKind::Reflexive,
                UdpCandidateKind::RouterMapped,
                UdpCandidateKind::Local,
            ],
            &["198.51.100.10:50000"],
        ));
        let peer = NatProfile::from_summary(&summary(
            "open/public",
            Some("stun.cloudflare.com:3478"),
            2,
            Some(true),
            &[UdpCandidateKind::Reflexive, UdpCandidateKind::Local],
            &["198.51.100.11:50001"],
        ));

        let plan = plan_for_pair(&local, &peer);
        let wire = plan.to_wire();

        assert_eq!(wire.mode, UdpAdaptiveMode::DirectFirst);
        assert_eq!(
            wire.candidate_order,
            vec![
                UdpAdaptiveCandidateKind::Local,
                UdpAdaptiveCandidateKind::Reflexive,
                UdpAdaptiveCandidateKind::RouterMapped,
                UdpAdaptiveCandidateKind::RelayFallback,
            ]
        );
        assert_eq!(wire.summary(), plan.summary());
    }

    // ---- Fase 3: structured wire profiles (no label parsing anywhere) ----

    fn wire_offer(kinds: &[UdpCandidateKind], stun: Option<&str>) -> UdpCandidateOffer {
        UdpCandidateOffer {
            candidates: kinds
                .iter()
                .enumerate()
                .map(|(i, _)| format!("198.51.100.10:{}", 40_000 + i).parse().unwrap())
                .collect(),
            selected_stun: stun.map(str::to_string),
            typed_candidates: kinds
                .iter()
                .enumerate()
                .map(|(i, kind)| crate::shared::UdpTypedCandidate {
                    addr: format!("198.51.100.10:{}", 40_000 + i).parse().unwrap(),
                    kind: *kind,
                    priority: 0,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn wire_profile(
        mapping: UdpNatMapping,
        filtering: UdpNatFiltering,
        port_preserved: Option<bool>,
        observations: u8,
    ) -> UdpNatProfile {
        UdpNatProfile {
            mapping,
            filtering,
            port_preserved,
            observations,
        }
    }

    /// Table over the structured profile pairs: every mode decision + stable
    /// reason code, no text label anywhere in the inputs.
    #[test]
    fn from_wire_pair_table() {
        use UdpCandidateKind as K;
        let eim = |pp| wire_profile(UdpNatMapping::Eim, UdpNatFiltering::Unknown, pp, 2);
        let sym = |pp| wire_profile(UdpNatMapping::Symmetric, UdpNatFiltering::Unknown, pp, 2);
        let unk = |obs| wire_profile(UdpNatMapping::Unknown, UdpNatFiltering::Unknown, None, obs);
        let both = wire_offer(&[K::Reflexive, K::Local], Some("stun.example:3478"));
        let no_stun_offer = wire_offer(&[K::Local], None);
        let empty = UdpCandidateOffer::default();

        // (local profile+offer, peer profile+offer, expected mode, reason)
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            UdpNatProfile,
            &UdpCandidateOffer,
            UdpNatProfile,
            &UdpCandidateOffer,
            NatPlanMode,
            &str,
        )> = vec![
            (
                eim(Some(true)),
                &both,
                eim(Some(true)),
                &both,
                NatPlanMode::DirectFirst,
                "both-direct-friendly",
            ),
            (
                eim(Some(true)),
                &both,
                sym(Some(true)),
                &both,
                NatPlanMode::DirectWithRetry,
                "symmetric-escape",
            ),
            (
                sym(None),
                &no_stun_offer,
                sym(None),
                &no_stun_offer,
                NatPlanMode::RelayFirst,
                "symmetric-relay",
            ),
            (
                unk(1),
                &both,
                eim(Some(true)),
                &both,
                NatPlanMode::DirectWithRetry,
                "inconclusive",
            ),
            // STUN dead on one side (no reflexive candidate, zero observations).
            (
                unk(0),
                &no_stun_offer,
                eim(Some(true)),
                &both,
                NatPlanMode::RelayFirst,
                "peer-blocked",
            ),
            (
                unk(0),
                &empty,
                unk(0),
                &empty,
                NatPlanMode::RelayOnly,
                "no-candidates",
            ),
        ];
        for (i, (lp, lo, pp, po, mode, code)) in rows.iter().enumerate() {
            let local = NatProfile::from_wire(lp, lo);
            let peer = NatProfile::from_wire(pp, po);
            let plan = plan_for_pair(&local, &peer);
            assert_eq!(plan.mode, *mode, "row {i} mode");
            assert_eq!(plan.reason_code, *code, "row {i} reason code");
            // The relay is always in the order: last in direct-led modes,
            // FIRST in relay-led ones.
            assert!(
                plan.candidate_order
                    .contains(&NatCandidateKind::RelayFallback),
                "row {i}: relay fallback missing from the order"
            );
            match mode {
                NatPlanMode::DirectFirst | NatPlanMode::DirectWithRetry => assert_eq!(
                    plan.candidate_order.last(),
                    Some(&NatCandidateKind::RelayFallback),
                    "row {i}: relay must be last in direct-led modes"
                ),
                NatPlanMode::RelayFirst | NatPlanMode::RelayOnly => assert_eq!(
                    plan.candidate_order.first(),
                    Some(&NatCandidateKind::RelayFallback),
                    "row {i}: relay must be first in relay-led modes"
                ),
            }
        }
    }

    /// Symmetric mapping + address/port-dependent filtering on the same side
    /// is the worst RFC 4787 combination: relay first, dedicated reason code.
    #[test]
    fn from_wire_symmetric_strict_filtering_goes_relay_first() {
        let offer = wire_offer(
            &[UdpCandidateKind::Reflexive, UdpCandidateKind::Local],
            Some("stun.example:3478"),
        );
        let apdm_apdf = wire_profile(
            UdpNatMapping::Symmetric,
            UdpNatFiltering::AddressDependent,
            Some(true),
            2,
        );
        let cone = wire_profile(UdpNatMapping::Eim, UdpNatFiltering::Unknown, Some(true), 2);
        let local = NatProfile::from_wire(&apdm_apdf, &offer);
        let peer = NatProfile::from_wire(&cone, &offer);
        let plan = plan_for_pair(&local, &peer);
        assert_eq!(plan.mode, NatPlanMode::RelayFirst);
        assert_eq!(plan.reason_code, "symmetric-strict-filtering");
    }

    /// Fase 5 gate: a manual/router-mapped candidate with `--udp-no-stun`
    /// (zero observations, no reflexive) must NOT classify as blocked — the
    /// operator declared a reachable endpoint; the pair plans direct.
    #[test]
    fn from_wire_manual_candidate_no_stun_is_not_blocked() {
        let offer = wire_offer(&[UdpCandidateKind::RouterMapped], None);
        let profile = wire_profile(UdpNatMapping::Unknown, UdpNatFiltering::Unknown, None, 0);
        let local = NatProfile::from_wire(&profile, &offer);
        assert_eq!(local.mapping_class, NatMappingClass::Inconclusive);
        let plan = plan_for_pair(&local, &local);
        assert_eq!(plan.mode, NatPlanMode::DirectWithRetry);
        assert_eq!(
            plan.candidate_order.first(),
            Some(&NatCandidateKind::RouterMapped),
            "the manual/router-mapped candidate leads the order"
        );
    }

    /// `Unknown` mapping must NEVER be read as hostile: with observations it
    /// is inconclusive (direct with retry), and even with zero observations a
    /// reflexive candidate keeps it off the blocked classification.
    #[test]
    fn from_wire_unknown_never_relay_only() {
        let offer = wire_offer(&[UdpCandidateKind::Reflexive], Some("stun.example:3478"));
        let unknown = wire_profile(UdpNatMapping::Unknown, UdpNatFiltering::Unknown, None, 0);
        let profile = NatProfile::from_wire(&unknown, &offer);
        // Reflexive candidate present ⇒ inconclusive, not blocked.
        assert_eq!(profile.mapping_class, NatMappingClass::Inconclusive);
        let plan = plan_for_pair(&profile, &profile);
        assert_ne!(plan.mode, NatPlanMode::RelayOnly);
        assert_ne!(plan.mode, NatPlanMode::RelayFirst);
    }
}
