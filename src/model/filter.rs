//! Search text and filter settings applied to the visible aircraft.

use super::aircraft::Aircraft;

#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub search: String,
    pub min_alt_ft: Option<f64>,
    pub max_alt_ft: Option<f64>,
    pub hide_ground: bool,
    pub military_only: bool,
    pub type_code: String,
}

impl Filter {
    pub fn matches(&self, a: &Aircraft) -> bool {
        if self.hide_ground && a.on_ground {
            return false;
        }
        if self.military_only && !a.military {
            return false;
        }
        let alt = a.alt_baro_ft.or(a.alt_geom_ft);
        if let Some(min) = self.min_alt_ft
            && alt.is_none_or(|v| v < min)
        {
            return false;
        }
        if let Some(max) = self.max_alt_ft
            && alt.is_none_or(|v| v > max)
        {
            return false;
        }
        let t = self.type_code.trim();
        if !t.is_empty()
            && !a
                .type_code
                .as_deref()
                .is_some_and(|c| c.eq_ignore_ascii_case(t))
        {
            return false;
        }
        let q = self.search.trim();
        if !q.is_empty() {
            let q = q.to_ascii_lowercase();
            let hit = |s: &Option<String>| {
                s.as_deref()
                    .is_some_and(|v| v.to_ascii_lowercase().contains(&q))
            };
            if !(a.hex.to_ascii_lowercase().contains(&q)
                || hit(&a.callsign)
                || hit(&a.registration))
            {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ac() -> Aircraft {
        Aircraft {
            hex: "a1b2c3".into(),
            callsign: Some("SWA123".into()),
            registration: Some("N123SW".into()),
            type_code: Some("B738".into()),
            alt_baro_ft: Some(30000.0),
            ..Default::default()
        }
    }

    #[test]
    fn search_fields() {
        for q in ["swa", "n123", "A1B2"] {
            let f = Filter {
                search: q.into(),
                ..Default::default()
            };
            assert!(f.matches(&ac()), "{q}");
        }
        let f = Filter {
            search: "zzz".into(),
            ..Default::default()
        };
        assert!(!f.matches(&ac()));
    }

    #[test]
    fn altitude_ground_mil_type() {
        let f = Filter {
            min_alt_ft: Some(35000.0),
            ..Default::default()
        };
        assert!(!f.matches(&ac()));
        let f = Filter {
            max_alt_ft: Some(35000.0),
            ..Default::default()
        };
        assert!(f.matches(&ac()));
        let f = Filter {
            military_only: true,
            ..Default::default()
        };
        assert!(!f.matches(&ac()));
        let f = Filter {
            type_code: "b738".into(),
            ..Default::default()
        };
        assert!(f.matches(&ac()));
        let g = Aircraft {
            on_ground: true,
            ..ac()
        };
        let f = Filter {
            hide_ground: true,
            ..Default::default()
        };
        assert!(!f.matches(&g));
    }
}
