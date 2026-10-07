//! A machine's health as one number: 100 minus weighted pressure. Each
//! metric contributes nothing until it crosses its warning level and its
//! full weight at its critical level, so an idle machine scores 100 and a
//! single saturated resource drags the score into the degraded band on its
//! own. Unknown sensors never cost anything. The reason is the metric
//! contributing the most, once that contribution is visible.

use serde_json::{Value, json};

use crate::output::{num, round1};

/// Everything the score reads.
#[derive(Clone, Copy, Debug, Default)]
pub struct Inputs {
    pub cpu: Option<f64>,
    pub mem: f64,
    pub swap: Option<f64>,
    pub disk: f64,
    pub load_per_core: f64,
    pub cpu_temp: Option<f64>,
    pub gpu_temp: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reason {
    pub metric: &'static str,
    pub value: f64,
}

fn ramp(value: f64, warn: f64, critical: f64) -> f64 {
    ((value - warn) / (critical - warn)).clamp(0.0, 1.0)
}

/// The score, 0 to 100, and the metric that cost the most.
pub fn assess(inputs: Inputs) -> (u8, Option<Reason>) {
    let pressure =
        |value: Option<f64>, warn, critical| value.map_or(0.0, |v| ramp(v, warn, critical));
    // Metric, weight, pressure, and the reading shown when it's the reason.
    let contributions: [(&str, f64, f64, f64); 7] = [
        (
            "cpu",
            35.0,
            pressure(inputs.cpu, 75.0, 98.0),
            inputs.cpu.unwrap_or(0.0),
        ),
        ("mem", 35.0, ramp(inputs.mem, 78.0, 96.0), inputs.mem),
        (
            "swap",
            15.0,
            pressure(inputs.swap, 50.0, 90.0),
            inputs.swap.unwrap_or(0.0),
        ),
        ("disk", 30.0, ramp(inputs.disk, 82.0, 96.0), inputs.disk),
        (
            "load",
            15.0,
            ramp(inputs.load_per_core, 1.0, 2.5),
            inputs.load_per_core,
        ),
        (
            "cpu_temp",
            20.0,
            pressure(inputs.cpu_temp, 82.0, 97.0),
            inputs.cpu_temp.unwrap_or(0.0),
        ),
        (
            "gpu_temp",
            15.0,
            pressure(inputs.gpu_temp, 82.0, 95.0),
            inputs.gpu_temp.unwrap_or(0.0),
        ),
    ];
    let mut penalty = 0.0;
    let mut worst: Option<(Reason, f64)> = None;
    for (metric, weight, pressure, value) in contributions {
        let contribution = weight * pressure;
        penalty += contribution;
        if contribution > 2.0 && worst.is_none_or(|(_, most)| contribution > most) {
            worst = Some((Reason { metric, value }, contribution));
        }
    }
    let score = (100.0 - penalty).clamp(0.0, 100.0).round() as u8;
    (score, worst.map(|(reason, _)| reason))
}

/// The band a score falls in.
pub fn status_for_score(score: u8) -> &'static str {
    if score >= 75 {
        "healthy"
    } else if score >= 45 {
        "degraded"
    } else {
        "critical"
    }
}

/// `{score, status, reason}` as records carry it.
pub fn record(inputs: Inputs) -> Value {
    let (score, reason) = assess(inputs);
    json!({
        "score": score,
        "status": status_for_score(score),
        "reason": reason.map(|reason| json!({
            "metric": reason.metric,
            "value": num(round1(reason.value)),
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scores worked by hand from the weights and ramps above.
    #[test]
    fn pressure_past_each_warning_level_costs_its_weight() {
        let idle = Inputs {
            cpu: Some(12.0),
            mem: 40.0,
            swap: Some(5.0),
            disk: 50.0,
            load_per_core: 0.2,
            cpu_temp: Some(45.0),
            gpu_temp: None,
        };
        assert_eq!(assess(idle), (100, None));
        // Disk 91%: 30 × (91 − 82) / 14 = 19.3, so 81, and disk is why.
        let disk = Inputs {
            disk: 91.0,
            mem: 30.0,
            ..Inputs::default()
        };
        assert_eq!(
            assess(disk),
            (
                81,
                Some(Reason {
                    metric: "disk",
                    value: 91.0
                })
            )
        );
        assert_eq!(status_for_score(81), "healthy");
        // Everything saturated floors at zero; cpu and mem tie at 35, and
        // the first to reach the top stays the reason.
        let (score, reason) = assess(Inputs {
            disk: 99.0,
            mem: 97.0,
            swap: Some(95.0),
            cpu: Some(100.0),
            ..Inputs::default()
        });
        assert_eq!((score, status_for_score(score)), (0, "critical"));
        assert_eq!(reason.map(|reason| reason.metric), Some("cpu"));
        // A contribution of 2 or less is not worth naming.
        let (score, reason) = assess(Inputs {
            load_per_core: 1.1,
            ..Inputs::default()
        });
        assert_eq!((score, reason), (99, None));
        // A Mac captured from a running fleet: swap 64.3% costs
        // 15 × 14.3 / 40 = 5.4 and disk 89.2% 30 × 7.2 / 14 = 15.4: 79.
        let (score, reason) = assess(Inputs {
            cpu: Some(17.9),
            mem: 64.1,
            swap: Some(64.3),
            disk: 89.2,
            load_per_core: 3.47 / 12.0,
            cpu_temp: None,
            gpu_temp: None,
        });
        assert_eq!(
            (score, reason.map(|reason| reason.metric)),
            (79, Some("disk"))
        );
        assert_eq!(
            (status_for_score(45), status_for_score(44)),
            ("degraded", "critical")
        );
    }
}
