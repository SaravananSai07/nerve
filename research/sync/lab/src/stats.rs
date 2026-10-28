pub(crate) fn report(label: &str, sent: u32, mut rtts: Vec<f64>) {
    let got = rtts.len();
    if rtts.is_empty() {
        println!("RESULT {label}: sent={sent} recv=0 loss=100%");
        return;
    }
    rtts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| rtts[((rtts.len() - 1) as f64 * p).round() as usize];
    let mean = rtts.iter().sum::<f64>() / rtts.len() as f64;
    println!(
        "RESULT {label}: sent={sent} recv={got} loss={:.1}% min={:.1} p50={:.1} mean={:.1} p95={:.1} max={:.1} (ms RTT)",
        100.0 * (sent as f64 - got as f64) / sent as f64,
        rtts[0],
        pct(0.5),
        mean,
        pct(0.95),
        rtts[rtts.len() - 1]
    );
}
