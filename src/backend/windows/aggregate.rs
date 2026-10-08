use super::parse::{Engine, engine_instance};
use std::collections::HashMap;

#[derive(Default)]
pub struct Utilization {
    pub util_by_luid: HashMap<String, f64>,
    pub enc_by_luid: HashMap<String, f64>,
    pub dec_by_luid: HashMap<String, f64>,
    pub util_by_proc: HashMap<(u32, String), f64>,
    pub proc_graphics: HashMap<(u32, String), bool>,
}

pub fn aggregate(samples: Vec<(String, f64)>) -> Utilization {
    let mut engines: HashMap<Engine, (String, f64)> = HashMap::new();
    let mut processes: HashMap<(u32, Engine), f64> = HashMap::new();
    let mut result = Utilization::default();
    for (instance, value) in samples {
        let Some((pid, engine, kind)) = engine_instance(&instance) else {
            continue;
        };
        if !value.is_finite() || value < 0.0 {
            continue;
        }
        let entry = engines.entry(engine.clone()).or_insert((kind.clone(), 0.0));
        entry.1 += value;
        *processes.entry((pid, engine.clone())).or_default() += value;
        *result.proc_graphics.entry((pid, engine.luid)).or_default() |=
            kind.contains("3d") || kind.contains("graphics");
    }
    for (engine, (kind, value)) in engines {
        if kind.contains("videoencode") {
            let entry = result.enc_by_luid.entry(engine.luid.clone()).or_default();
            *entry = entry.max(value);
        } else if kind.contains("videodecode") {
            let entry = result.dec_by_luid.entry(engine.luid.clone()).or_default();
            *entry = entry.max(value);
        }
        let entry = result.util_by_luid.entry(engine.luid).or_default();
        *entry = entry.max(value);
    }
    for ((pid, engine), value) in processes {
        let entry = result.util_by_proc.entry((pid, engine.luid)).or_default();
        *entry = entry.max(value);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &str = "luid_0x00000000_0x00000001";
    const B: &str = "luid_0x00000000_0x00000002";

    fn sample(pid: u32, luid: &str, phys: u32, eng: u32, kind: &str, v: f64) -> (String, f64) {
        (
            format!("pid_{pid}_{luid}_phys_{phys}_eng_{eng}_engtype_{kind}"),
            v,
        )
    }

    #[test]
    fn processes_sum_on_one_engine_but_independent_engines_take_max() {
        let r = aggregate(vec![
            sample(1, A, 0, 0, "3d", 20.),
            sample(2, A, 0, 0, "3d", 30.),
            sample(1, A, 0, 1, "3d", 40.),
            sample(1, A, 1, 0, "3d", 45.),
            sample(1, B, 0, 0, "3d", 70.),
        ]);
        assert_eq!(r.util_by_luid[A], 50.);
        assert_eq!(r.util_by_luid[B], 70.);
        assert_eq!(r.util_by_proc[&(1, A.into())], 45.);
        assert_eq!(r.util_by_proc[&(2, A.into())], 30.);
        assert_eq!(r.util_by_proc[&(1, B.into())], 70.);
        assert!(r.proc_graphics[&(1, A.into())]);
    }

    #[test]
    fn video_metrics_take_busiest_physical_engine() {
        let r = aggregate(vec![
            sample(1, A, 0, 0, "videoencode", 20.),
            sample(2, A, 0, 0, "videoencode", 30.),
            sample(1, A, 0, 1, "videoencode", 40.),
            sample(1, A, 0, 2, "videodecode", 15.),
            sample(1, A, 0, 3, "videodecode", 25.),
        ]);
        assert_eq!(r.enc_by_luid[A], 50.);
        assert_eq!(r.dec_by_luid[A], 25.);
    }

    #[test]
    fn only_process_instances_contribute_and_missing_stays_unknown() {
        let r = aggregate(vec![
            (format!("{A}_phys_0_eng_0_engtype_3d"), 90.),
            sample(1, A, 0, 0, "3d", 20.),
        ]);
        assert_eq!(r.util_by_luid[A], 20.);
        assert!(!r.util_by_luid.contains_key(B));
        assert!(!r.enc_by_luid.contains_key(A));
        assert!(aggregate(Vec::new()).util_by_luid.is_empty());
    }
}
