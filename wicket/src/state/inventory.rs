// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Information about all top-level Oxide components (sleds, switches, PSCs)

use anyhow::anyhow;
use once_cell::sync::Lazy;
use ratatui::text::Text;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Display;
use std::iter::Iterator;
use wicketd_client::types::{
    RackV1Inventory, RotInventory, RotSlot, SpComponentCaboose,
    SpComponentInfo, SpIgnition, SpState, SpType,
};

pub static ALL_COMPONENT_IDS: Lazy<Vec<ComponentId>> = Lazy::new(|| {
    (0..=31u8)
        .map(ComponentId::Sled)
        .chain((0..=1u8).map(ComponentId::Switch))
        // Currently shipping racks don't have PSC 1.
        .chain(std::iter::once(ComponentId::Psc(0)))
        .collect()
});

/// Inventory is the most recent information about rack composition as
/// received from MGS.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Inventory {
    power: BTreeMap<ComponentId, PowerState>,
    inventory: BTreeMap<ComponentId, Component>,
}

impl Inventory {
    pub fn get_power_state(&self, id: &ComponentId) -> Option<&PowerState> {
        self.power.get(id)
    }

    pub fn get_inventory(&self, id: &ComponentId) -> Option<&Component> {
        self.inventory.get(id)
    }

    pub fn components(&self) -> impl Iterator<Item = &ComponentId> {
        self.inventory.keys()
    }

    pub fn update_inventory(
        &mut self,
        inventory: RackV1Inventory,
    ) -> anyhow::Result<()> {
        let mut new_inventory = Inventory::default();

        for sp in inventory.sps {
            let i = sp.id.slot;
            let type_ = sp.id.type_;
            let sp = Sp {
                ignition: sp.ignition,
                state: sp.state,
                caboose_active: sp.caboose_active,
                caboose_inactive: sp.caboose_inactive,
                components: sp.components,
                rot: sp.rot,
            };

            // Validate and get a ComponentId
            let (id, component) = match type_ {
                SpType::Sled => {
                    if i > 31 {
                        return Err(anyhow!("Invalid sled slot: {}", i));
                    }
                    (ComponentId::Sled(i as u8), Component::Sled(sp))
                }
                SpType::Switch => {
                    if i > 1 {
                        return Err(anyhow!("Invalid switch slot: {}", i));
                    }
                    (ComponentId::Switch(i as u8), Component::Switch(sp))
                }
                SpType::Power => {
                    if i > 1 {
                        return Err(anyhow!("Invalid power shelf slot: {}", i));
                    }
                    (ComponentId::Psc(i as u8), Component::Psc(sp))
                }
            };
            new_inventory.inventory.insert(id, component);

            // TODO: Plumb through real power state
            new_inventory.power.insert(id, PowerState::A2);
        }

        self.inventory = new_inventory.inventory;
        self.power = new_inventory.power;

        Ok(())
    }
}

// We just print the debug info on the screen for now
#[allow(unused)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sp {
    ignition: Option<SpIgnition>,
    state: Option<SpState>,
    caboose_active: Option<SpComponentCaboose>,
    caboose_inactive: Option<SpComponentCaboose>,
    components: Option<Vec<SpComponentInfo>>,
    rot: Option<RotInventory>,
}

impl Sp {
    pub fn ignition(&self) -> Option<&SpIgnition> {
        self.ignition.as_ref()
    }

    pub fn state(&self) -> Option<&SpState> {
        self.state.as_ref()
    }

    pub fn caboose_active(&self) -> Option<&SpComponentCaboose> {
        self.caboose_active.as_ref()
    }

    pub fn caboose_inactive(&self) -> Option<&SpComponentCaboose> {
        self.caboose_inactive.as_ref()
    }

    pub fn rot(&self) -> Option<&RotInventory> {
        self.rot.as_ref()
    }

    pub fn components(&self) -> &[SpComponentInfo] {
        match self.components.as_ref() {
            Some(components) => components,
            None => &[],
        }
    }
}

// XXX: Eventually a Sled will have a host component.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Component {
    Sled(Sp),
    Switch(Sp),
    Psc(Sp),
}

fn version_or_unknown(caboose: Option<&SpComponentCaboose>) -> String {
    caboose.and_then(|c| c.version.as_deref()).unwrap_or("UNKNOWN").to_string()
}

impl Component {
    pub fn sp(&self) -> &Sp {
        match self {
            Component::Sled(sp) => sp,
            Component::Switch(sp) => sp,
            Component::Psc(sp) => sp,
        }
    }

    pub fn sp_version_active(&self) -> String {
        version_or_unknown(self.sp().caboose_active.as_ref())
    }

    pub fn sp_version_inactive(&self) -> String {
        version_or_unknown(self.sp().caboose_inactive.as_ref())
    }

    pub fn rot_active_slot(&self) -> Option<RotSlot> {
        self.sp().rot.as_ref().map(|rot| rot.active)
    }

    pub fn rot_version_a(&self) -> String {
        version_or_unknown(
            self.sp().rot.as_ref().and_then(|rot| rot.caboose_a.as_ref()),
        )
    }

    pub fn rot_version_b(&self) -> String {
        version_or_unknown(
            self.sp().rot.as_ref().and_then(|rot| rot.caboose_b.as_ref()),
        )
    }
}

// The component type and its slot.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialOrd,
    Ord,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
)]
pub enum ComponentId {
    Sled(u8),
    Switch(u8),
    Psc(u8),
}

impl ComponentId {
    pub fn name(&self) -> String {
        self.to_string()
    }
}

impl Display for ComponentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComponentId::Sled(i) => write!(f, "SLED {}", i),
            ComponentId::Switch(i) => write!(f, "SWITCH {}", i),
            ComponentId::Psc(i) => write!(f, "PSC {}", i),
        }
    }
}

impl From<ComponentId> for Text<'_> {
    fn from(value: ComponentId) -> Self {
        value.to_string().into()
    }
}

pub struct ParsableComponentId<'a> {
    pub sp_type: &'a str,
    pub i: &'a str,
}

impl<'a> TryFrom<ParsableComponentId<'a>> for ComponentId {
    type Error = ();
    fn try_from(value: ParsableComponentId<'a>) -> Result<Self, Self::Error> {
        let i: u8 = value.i.parse().map_err(|_| ())?;
        match (value.sp_type, i) {
            ("sled", 0..=31) => Ok(ComponentId::Sled(i)),
            ("switch", 0..=1) => Ok(ComponentId::Switch(i)),
            ("power", 0..=1) => Ok(ComponentId::Psc(i)),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub enum PowerState {
    /// Working
    A0,
    /// Sojourning
    A1,
    /// Quiescent
    A2,
    /// Commanded Off
    A3,
    /// Mechanical Off
    A4,
}

impl PowerState {
    pub fn description(&self) -> &'static str {
        match self {
            PowerState::A0 => "working",
            PowerState::A1 => "sojourning",
            PowerState::A2 => "quiescent",
            PowerState::A3 => "commanded off",
            PowerState::A4 => "mechanical off (unplugged)",
        }
    }
}
