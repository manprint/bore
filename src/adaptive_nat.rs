#![allow(missing_docs)]

use crate::shared::{
    UdpAdaptiveCandidateKind, UdpAdaptiveMode, UdpAdaptivePlan, UdpCandidateKind,
    UdpTestPeerSummary,
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
    pub(crate) local_udp: String,
    pub(crate) selected_stun: Option<String>,
    pub(crate) candidate_kinds: Vec<NatCandidateKind>,
    pub(crate) candidate_count: usize,
    pub(crate) reflexive_count: usize,
    pub(crate) port_preserved: Option<bool>,
}

impl NatProfile {
    pub(crate) fn from_summary(summary: &UdpTestPeerSummary) -> Self {
        let mapping_class = NatMappingClass::from_label(&summary.nat_class);
        Self {
            mapping_class,
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
    let mode = select_mode(local, peer);
    let candidate_order = candidate_order(local, peer, mode);
    let (retry_budget, read_timeout_ms, send_delay_ms) = match mode {
        NatPlanMode::DirectFirst => (1, 750, 0),
        NatPlanMode::DirectWithRetry => (2, 500, 25),
        NatPlanMode::RelayFirst => (1, 250, 0),
        NatPlanMode::RelayOnly => (0, 0, 0),
    };
    let reasons = match mode {
        NatPlanMode::DirectFirst => {
            vec!["both peers look endpoint-independent or public".to_string()]
        }
        NatPlanMode::DirectWithRetry => {
            vec!["direct path looks plausible but one peer needs extra retry room".to_string()]
        }
        NatPlanMode::RelayFirst => vec![
            "one peer is blocked, symmetric, or inconclusive, so relay stays first-choice fallback"
                .to_string(),
        ],
        NatPlanMode::RelayOnly => {
            vec!["no usable direct candidates were reported by either peer".to_string()]
        }
    };

    NatPlan {
        mode,
        candidate_order,
        retry_budget,
        read_timeout_ms,
        send_delay_ms,
        reasons,
    }
}

fn select_mode(local: &NatProfile, peer: &NatProfile) -> NatPlanMode {
    if local.candidate_count == 0 && peer.candidate_count == 0 {
        return NatPlanMode::RelayOnly;
    }

    if local.mapping_class.direct_friendly() && peer.mapping_class.direct_friendly() {
        return NatPlanMode::DirectFirst;
    }

    if local.mapping_class.blocked() || peer.mapping_class.blocked() {
        return NatPlanMode::RelayFirst;
    }

    if local.mapping_class.symmetric() || peer.mapping_class.symmetric() {
        if local.port_preserved == Some(true)
            || peer.port_preserved == Some(true)
            || local.selected_stun.is_some()
            || peer.selected_stun.is_some()
        {
            return NatPlanMode::DirectWithRetry;
        }
        return NatPlanMode::RelayFirst;
    }

    if local.mapping_class.inconclusive() || peer.mapping_class.inconclusive() {
        return NatPlanMode::DirectWithRetry;
    }

    NatPlanMode::DirectWithRetry
}

fn candidate_order(
    local: &NatProfile,
    peer: &NatProfile,
    mode: NatPlanMode,
) -> Vec<NatCandidateKind> {
    let mut direct = Vec::new();
    if !local.candidate_kinds.is_empty() || !peer.candidate_kinds.is_empty() {
        for kind in [
            NatCandidateKind::Reflexive,
            NatCandidateKind::Local,
            NatCandidateKind::RouterMapped,
            NatCandidateKind::Predicted,
        ] {
            if local.has_candidate_kind(kind) || peer.has_candidate_kind(kind) {
                direct.push(kind);
            }
        }
    } else {
        if local.reflexive_count > 0 || peer.reflexive_count > 0 {
            direct.push(NatCandidateKind::Reflexive);
        }
        if !local.local_udp.is_empty() || !peer.local_udp.is_empty() {
            direct.push(NatCandidateKind::Local);
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
                NatCandidateKind::Reflexive,
                NatCandidateKind::Local,
                NatCandidateKind::RouterMapped,
                NatCandidateKind::RelayFallback,
            ]
        );
        assert_eq!(plan.retry_budget, 1);
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
        assert!(summary.contains("reflexive -> local -> router-mapped"));
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
                UdpAdaptiveCandidateKind::Reflexive,
                UdpAdaptiveCandidateKind::Local,
                UdpAdaptiveCandidateKind::RouterMapped,
                UdpAdaptiveCandidateKind::RelayFallback,
            ]
        );
        assert_eq!(wire.summary(), plan.summary());
    }
}
