use super::CapabilityState;
use crate::tool_manager::model::{AdapterKind, DeliveryId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityGate {
    pub adapter: AdapterKind,
    pub phase_b: CapabilityState,
    pub phase_c: CapabilityState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackingGate {
    pub adapter: AdapterKind,
    pub delivery: DeliveryId,
    pub phase_b: CapabilityState,
    pub phase_c: CapabilityState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateRow {
    PackageUpdate(CapabilityGate),
    Tracking(TrackingGate),
}

pub const GATE_TABLE: [GateRow; 13] = [
    GateRow::PackageUpdate(CapabilityGate {
        adapter: AdapterKind::Cargo,
        phase_b: CapabilityState::Gated,
        phase_c: CapabilityState::Gated,
    }),
    GateRow::PackageUpdate(CapabilityGate {
        adapter: AdapterKind::UvTool,
        phase_b: CapabilityState::Gated,
        phase_c: CapabilityState::Gated,
    }),
    GateRow::PackageUpdate(CapabilityGate {
        adapter: AdapterKind::Pipx,
        phase_b: CapabilityState::Gated,
        phase_c: CapabilityState::Gated,
    }),
    GateRow::PackageUpdate(CapabilityGate {
        adapter: AdapterKind::InterpreterPip,
        phase_b: CapabilityState::Gated,
        phase_c: CapabilityState::Gated,
    }),
    GateRow::PackageUpdate(CapabilityGate {
        adapter: AdapterKind::NpmGlobal,
        phase_b: CapabilityState::Gated,
        phase_c: CapabilityState::Gated,
    }),
    GateRow::PackageUpdate(CapabilityGate {
        adapter: AdapterKind::PnpmGlobal,
        phase_b: CapabilityState::Gated,
        phase_c: CapabilityState::Gated,
    }),
    GateRow::PackageUpdate(CapabilityGate {
        adapter: AdapterKind::BrewFormula,
        phase_b: CapabilityState::Gated,
        phase_c: CapabilityState::Gated,
    }),
    GateRow::PackageUpdate(CapabilityGate {
        adapter: AdapterKind::BrewCask,
        phase_b: CapabilityState::Gated,
        phase_c: CapabilityState::Gated,
    }),
    GateRow::PackageUpdate(CapabilityGate {
        adapter: AdapterKind::WingetOffline,
        phase_b: CapabilityState::Gated,
        phase_c: CapabilityState::Gated,
    }),
    GateRow::PackageUpdate(CapabilityGate {
        adapter: AdapterKind::WingetOnline,
        phase_b: CapabilityState::Gated,
        phase_c: CapabilityState::Gated,
    }),
    GateRow::PackageUpdate(CapabilityGate {
        adapter: AdapterKind::CcyncReceipt,
        phase_b: CapabilityState::Gated,
        phase_c: CapabilityState::Gated,
    }),
    GateRow::PackageUpdate(CapabilityGate {
        adapter: AdapterKind::ManualPath,
        phase_b: CapabilityState::Gated,
        phase_c: CapabilityState::Gated,
    }),
    GateRow::Tracking(TrackingGate {
        adapter: AdapterKind::ManualPath,
        delivery: DeliveryId::ObserveOnly,
        phase_b: CapabilityState::Gated,
        phase_c: CapabilityState::Gated,
    }),
];

pub fn package_update_state(kind: AdapterKind) -> CapabilityState {
    package_update_state_in(&GATE_TABLE, kind)
}

fn package_update_state_in(table: &[GateRow], kind: AdapterKind) -> CapabilityState {
    table
        .iter()
        .find_map(|row| match row {
            GateRow::PackageUpdate(g) if g.adapter == kind => Some(g.phase_b),
            _ => None,
        })
        .unwrap_or(CapabilityState::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn production_table_allows_only_the_paired_two_row_candidate() {
        let cargo = package_update_state(AdapterKind::Cargo);
        assert!(matches!(
            cargo,
            CapabilityState::Gated | CapabilityState::Supported
        ));
        assert_eq!(package_update_state(AdapterKind::InterpreterPip), cargo);
        for row in GATE_TABLE {
            let (b, c, expected) = match row {
                GateRow::PackageUpdate(g) => (
                    g.phase_b,
                    g.phase_c,
                    if matches!(g.adapter, AdapterKind::Cargo | AdapterKind::InterpreterPip) {
                        cargo
                    } else {
                        CapabilityState::Gated
                    },
                ),
                GateRow::Tracking(g) => (g.phase_b, g.phase_c, CapabilityState::Gated),
            };
            assert_eq!((b, c), (expected, expected));
        }
    }

    #[test]
    fn lookup_exercises_closed_and_two_row_candidate_policy() {
        for state in [CapabilityState::Gated, CapabilityState::Supported] {
            let mut table = GATE_TABLE;
            for row in &mut table {
                if let GateRow::PackageUpdate(g) = row {
                    let expected =
                        if matches!(g.adapter, AdapterKind::Cargo | AdapterKind::InterpreterPip) {
                            state
                        } else {
                            CapabilityState::Gated
                        };
                    g.phase_b = expected;
                    g.phase_c = expected;
                }
            }
            for row in table {
                if let GateRow::PackageUpdate(g) = row {
                    let expected =
                        if matches!(g.adapter, AdapterKind::Cargo | AdapterKind::InterpreterPip) {
                            state
                        } else {
                            CapabilityState::Gated
                        };
                    assert_eq!(package_update_state_in(&table, g.adapter), expected);
                    assert_eq!((g.phase_b, g.phase_c), (expected, expected));
                }
            }
            assert_eq!(
                package_update_state_in(&[], AdapterKind::Cargo),
                CapabilityState::Unsupported
            );
        }
    }

    #[test]
    fn manual_path_tracking_gate_is_consumed_observe_only_row() {
        let tracking_rows: Vec<_> = GATE_TABLE
            .iter()
            .filter_map(|row| match row {
                GateRow::Tracking(g) => Some(g),
                GateRow::PackageUpdate(_) => None,
            })
            .collect();

        assert_eq!(tracking_rows.len(), 1);
        assert_eq!(tracking_rows[0].adapter, AdapterKind::ManualPath);
        assert_eq!(tracking_rows[0].delivery, DeliveryId::ObserveOnly);
    }
}
