use std::sync::{Mutex, OnceLock};

fn sink() -> &'static Mutex<Vec<String>> {
    static SINK: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    SINK.get_or_init(|| Mutex::new(Vec::new()))
}

pub fn emit(s: impl AsRef<str>) {
    let s = s.as_ref().to_string();
    println!("{s}");
    if let Ok(mut g) = sink().lock() {
        let ts = chrono::Local::now().format("%H:%M:%S%.3f");
        g.push(format!("{ts}  {s}"));
        if g.len() > 2500 {
            g.drain(0..600);
        }
    }
}

pub fn snapshot(from: usize) -> (Vec<String>, usize) {
    match sink().lock() {
        Ok(g) => {
            let total = g.len();
            let start = from.min(total);
            (g[start..].to_vec(), total)
        }
        Err(_) => (vec![], 0),
    }
}

pub fn clear() {
    if let Ok(mut g) = sink().lock() {
        g.clear();
    }
}

#[macro_export]
macro_rules! outln {
    ($($t:tt)*) => {{
        $crate::logbuf::emit(format!($($t)*));
    }};
}
