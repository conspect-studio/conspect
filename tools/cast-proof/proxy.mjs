// TLS front for the engine's plain-HTTP control listener (run.sh, engine mode). A real deployment terminates TLS
// in front of the engine too; Chrome needs an https receiver page (CAF loads Shaka from a protocol-relative URL).
// Transparent: method, path, headers and body pass unchanged both ways (only Host is rewritten), so every answer
// the browser sees is the engine's own route. Several ports = several origins (the hosted-receiver setup: page on
// origin A, stream and reports on origin B).
//   node proxy.mjs --upstream http://[::1]:PORT --tls-key F --tls-cert F --ports 0,0 [--log <file.jsonl>]
// Prints `LISTENING {"origins":["https://localhost:A","https://localhost:B"]}`. The request log holds one JSON line
// per request: {at, port, method, path (cast tokens cut to 6 characters), status}.
import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';

const argv = process.argv.slice(2); const o = { ports: '0' };
for (let i = 0; i < argv.length; i++) o[argv[i].replace(/^--/, '')] = argv[++i];
if (!o.upstream || !o['tls-key'] || !o['tls-cert']) { console.error('usage: proxy.mjs --upstream URL --tls-key F --tls-cert F [--ports 0,0] [--log F]'); process.exit(2); }
const up = new URL(o.upstream);
const log = o.log ? fs.createWriteStream(o.log, { flags: 'a' }) : null;
const redact = (p) => p.replace(/^\/cast\/([A-Za-z0-9_-]{6})[A-Za-z0-9_-]{7,}/, '/cast/$1...');
const agent = new http.Agent({ keepAlive: true, maxSockets: 64 });

function handler(port) {
  return (req, res) => {
    const headers = { ...req.headers, host: up.host };
    const fwd = http.request({ hostname: up.hostname.replace(/^\[|\]$/g, ''), port: up.port, method: req.method, path: req.url, headers, agent, timeout: 30000 }, (r) => {
      res.writeHead(r.statusCode, r.statusMessage, r.rawHeaders);
      r.pipe(res);
      if (log) log.write(JSON.stringify({ at: Date.now(), port, method: req.method, path: redact(req.url.split('?')[0]), status: r.statusCode }) + '\n');
    });
    fwd.on('timeout', () => fwd.destroy(new Error('upstream timeout')));
    fwd.on('error', () => {
      if (log) log.write(JSON.stringify({ at: Date.now(), port, method: req.method, path: redact(req.url.split('?')[0]), status: 502 }) + '\n');
      if (!res.headersSent) { res.writeHead(502); } res.end();
    });
    req.pipe(fwd);
  };
}

const tls = { key: fs.readFileSync(o['tls-key']), cert: fs.readFileSync(o['tls-cert']) };
const servers = [];
for (const want of String(o.ports).split(',')) {
  const srv = https.createServer(tls);
  await new Promise((resolve, reject) => { srv.once('error', reject); srv.listen(Number(want) || 0, '::', resolve); });
  srv.on('request', handler(srv.address().port));
  servers.push(srv);
}
console.log('LISTENING ' + JSON.stringify({ origins: servers.map((s) => `https://localhost:${s.address().port}`) }));
const bye = () => { for (const s of servers) { s.closeAllConnections(); s.close(); } if (log) log.end(); setTimeout(() => process.exit(0), 200).unref(); };
process.on('SIGINT', bye); process.on('SIGTERM', bye);
