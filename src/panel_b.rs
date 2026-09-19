
async fn spawn_job<F>(stream: &mut TcpStream, name: &str, fut: F) -> Result<()>
where
    F: std::future::Future<Output = Result<()>> + Send + 'static,
{
    if BUSY.swap(true, Ordering::SeqCst) {
        return json_ok(stream, json!({"ok": false, "error": "job already running"})).await;
    }
    let name = name.to_string();
    let name_job = name.clone();
    tokio::spawn(async move {
        let name = name_job;
        let res = fut.await;
        match res {
            Ok(()) => outln!("[{name}] done"),
            Err(e) => outln!("[{name}] ERROR {e}"),
        }
        BUSY.store(false, Ordering::SeqCst);
    });
    json_ok(stream, json!({"ok": true, "started": name})).await
}

async fn json_ok(stream: &mut TcpStream, v: Value) -> Result<()> {
    let body = serde_json::to_vec(&v)?;
    respond(stream, 200, "application/json", &body).await
}

async fn respond(stream: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) -> Result<()> {
    let reason = match code {
        200 => "OK",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

const PANEL_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>OpenSea FCFS panel</title>
<style>
  :root { --bg:#0b0d10; --card:#14181e; --line:#2a323c; --txt:#e8eef4; --mut:#8b98a5; --wl:#3dd68c; --pub:#6ea8fe; --bad:#ff6b6b; --warn:#ffc857; }
  * { box-sizing: border-box; }
  body { margin:0; font: 14px/1.45 ui-sans-serif, system-ui, sans-serif; background:var(--bg); color:var(--txt); }
  header { padding:16px 20px; border-bottom:1px solid var(--line); display:flex; gap:16px; align-items:center; flex-wrap:wrap; }
  h1 { font-size:16px; margin:0; letter-spacing:.02em; }
  .mode { display:flex; border:1px solid var(--line); border-radius:8px; overflow:hidden; }
  .mode button { background:transparent; color:var(--mut); border:0; padding:8px 16px; cursor:pointer; font-weight:600; }
  .mode button.on.wl { background:#123d2a; color:var(--wl); }
  .mode button.on.pub { background:#1a2740; color:var(--pub); }
  main { display:grid; grid-template-columns: 360px 1fr; min-height: calc(100vh - 58px); }
  @media (max-width: 860px) { main { grid-template-columns: 1fr; } }
  form { padding:18px; display:flex; flex-direction:column; gap:10px; border-right:1px solid var(--line); }
  label { font-size:11px; color:var(--mut); text-transform:uppercase; letter-spacing:.06em; }
  input[type=text], input[type=number], input[type=datetime-local] {
    width:100%; background:var(--card); border:1px solid var(--line); color:var(--txt);
    padding:8px 10px; border-radius:6px;
  }
  .row { display:flex; gap:8px; }
  .row > * { flex:1; }
  .checks { display:flex; gap:16px; align-items:center; color:var(--mut); }
  .btns { display:grid; grid-template-columns:1fr 1fr; gap:8px; margin-top:6px; }
  button.act { border:0; border-radius:6px; padding:10px; font-weight:700; cursor:pointer; color:#08110c; }
  #bDoctor { background:#9aa4b2; }
  #bArm { background:var(--pub); }
  #bFire { background:#c4a574; }
  #bSnipe { background:var(--wl); }
  aside { padding:16px 18px; display:flex; flex-direction:column; min-height:0; }
  #status { font-size:12px; color:var(--mut); margin-bottom:8px; font-family: ui-monospace, monospace; white-space:pre-wrap; }
  #log { flex:1; background:#080a0c; border:1px solid var(--line); border-radius:8px; padding:10px;
         overflow:auto; font:12px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace; white-space:pre-wrap; min-height:280px; }
  .ok { color:var(--wl); } .err { color:var(--bad); } .warn { color:var(--warn); }
  .hint { color:var(--mut); font-size:12px; }
</style>
</head>
<body>
<header>
  <h1>OpenSea FCFS</h1>
  <div class="mode">
    <button type="button" id="mWL" class="on wl">WL / signed</button>
    <button type="button" id="mPub" class="pub">Public</button>
  </div>
  <span class="hint" id="hint">WL: OpenSea API to/data/value → instant EIP-1559 sign → multi-RPC · Public: pre-sign mintPublicDrop</span>
</header>
<main>
<form id="f" onsubmit="return false">
  <div id="slugWrap">
    <label>Collection slug (WL → api-snipe)</label>
    <input type="text" id="slug" placeholder="theroyalmechanica" value="theroyalmechanica"/>
    <p class="hint" id="wlNote">OpenSea <code>POST /drops/{slug}/mint</code> returns <b>to / data / value</b>; that calldata is signed locally (no estimateGas) and fan-out to all RPCs.</p>
  </div>
  <div id="nftWrap" style="display:none">
    <label>NFT address (Public)</label>
    <input type="text" id="nft" placeholder="0x…"/>
  </div>
  <div class="row">
    <div><label>Qty</label><input type="number" id="qty" min="1" value="1"/></div>
    <div><label>early-ms</label><input type="number" id="early" value="50"/></div>
  </div>
  <label>Go-time (unix sec/ms or IST YYYY-MM-DD HH:MM:SS)</label>
  <input type="text" id="at" placeholder="1770000000 or 2026-09-13 16:00:00"/>
  <label>IST helper → fills go-time</label>
  <input type="datetime-local" id="ist"/>
  <div class="checks">
    <label><input type="checkbox" id="dry" checked/> dry-run</label>
    <label><input type="checkbox" id="yes"/> yes / live</label>
  </div>
  <div class="btns">
    <button class="act" id="bDoctor" type="button">Doctor</button>
    <button class="act" id="bArm" type="button">Arm</button>
    <button class="act" id="bFire" type="button">Fire</button>
    <button class="act" id="bSnipe" type="button">Snipe</button>
  </div>
  <p class="hint">Reads local .env. Private key never shown. Live snipe requires <b>yes</b> (uncheck dry-run).</p>
</form>
<aside>
  <div id="status">loading…</div>
  <div id="log"></div>
</aside>
</main>
<script>
let mode = 'wl';
let logFrom = 0;
const $ = id => document.getElementById(id);
function setMode(m) {
  mode = m;
  $('mWL').classList.toggle('on', m==='wl');
  $('mPub').classList.toggle('on', m==='public');
  $('slugWrap').style.display = m==='wl' ? '' : 'none';
  $('nftWrap').style.display = m==='public' ? '' : 'none';
  $('hint').textContent = m==='wl'
    ? 'WL: OpenSea mint JSON (to/data/value) drives calldata → sign → multi-RPC (api-snipe)'
    : 'Public: pre-sign mintPublicDrop · no OpenSea API on hot path';
}
$('mWL').onclick = () => setMode('wl');
$('mPub').onclick = () => setMode('public');
$('ist').addEventListener('change', () => {
  const v = $('ist').value;
  if (!v) return;
  $('at').value = v.replace('T',' ') + ':00'.slice(0, v.length>=16?0:3);
  if ($('at').value.length === 16) $('at').value += ':00';
});
function body() {
  return {
    mode, slug: $('slug').value.trim(), nft: $('nft').value.trim(),
    qty: Number($('qty').value)||1, at: $('at').value.trim(),
    early_ms: Number($('early').value)||0,
    dry_run: $('dry').checked, yes: $('yes').checked
  };
}
async function post(url, payload) {
  const r = await fetch(url, {method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify(payload||{})});
  return r.json();
}
$('bDoctor').onclick = () => post('/api/doctor', {});
$('bArm').onclick = () => post('/api/arm', body());
$('bFire').onclick = () => post('/api/fire', body());
$('bSnipe').onclick = () => post('/api/snipe', body());
function paintStatus(s) {
  if (!s.ok) { $('status').innerHTML = '<span class="err">config: '+esc(s.error||'err')+'</span>'; return; }
  $('status').innerHTML =
    'wallet <span class="ok">'+esc(s.wallet)+'</span>  chain='+s.chain_id+
    '  rpcs='+s.rpcs+'  api_key='+(s.has_api_key?'<span class="ok">yes</span>':'<span class="err">missing</span>')+
    '  wl=<span class="ok">OpenSea calldata</span>'+
    '  gas='+s.gas_limit+'  tip='+s.priority_fee_gwei+'gwei'+
    (s.busy?'  <span class="warn">BUSY</span>':'');
}
function esc(t){ return String(t).replace(/[&<>]/g, c=>({ '&':'&amp;','<':'&lt;','>':'&gt;' }[c])); }
function lineClass(l){
  if (/ERROR|fatal|auth failed/i.test(l)) return 'err';
  if (/WARN|still pending/i.test(l)) return 'warn';
  if (/READY|submitted|done|armed ok/i.test(l)) return 'ok';
  return '';
}
async function tick() {
  try {
    const s = await (await fetch('/api/status')).json();
    paintStatus(s);
    const lg = await (await fetch('/api/logs?from='+logFrom)).json();
    if (lg.lines && lg.lines.length) {
      const el = $('log');
      lg.lines.forEach(l => {
        const d = document.createElement('div');
        d.className = lineClass(l);
        d.textContent = l;
        el.appendChild(d);
      });
      logFrom = lg.total;
      el.scrollTop = el.scrollHeight;
    }
  } catch(e) {}
}
setInterval(tick, 250);
tick();
</script>
</body>
</html>
"##;
