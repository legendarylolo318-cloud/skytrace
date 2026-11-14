//! Lazily fetched aircraft / route metadata (from adsbdb.com).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AircraftInfo {
    pub registration: Option<String>,
    pub type_name: Option<String>,
    pub icao_type: Option<String>,
    pub manufacturer: Option<String>,
    pub operator: Option<String>,
    pub country: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Airport {
    pub iata: Option<String>,
    pub icao: Option<String>,
    pub name: Option<String>,
    pub municipality: Option<String>,
}

impl Airport {
    pub fn label(&self) -> String {
        let code = self.iata.as_deref().or(self.icao.as_deref()).unwrap_or("?");
        match self.municipality.as_deref().or(self.name.as_deref()) {
            Some(place) => format!("{code} ({place})"),
            None => code.to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RouteInfo {
    pub airline: Option<String>,
    pub origin: Option<Airport>,
    pub destination: Option<Airport>,
}

impl RouteInfo {
    pub fn summary(&self) -> Option<String> {
        match (&self.origin, &self.destination) {
            (Some(o), Some(d)) => Some(format!("{} → {}", o.label(), d.label())),
            (Some(o), None) => Some(format!("{} → ?", o.label())),
            (None, Some(d)) => Some(format!("? → {}", d.label())),
            (None, None) => None,
        }
    }
}
