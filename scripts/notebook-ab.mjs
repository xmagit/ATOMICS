#!/usr/bin/env node
// Controlled fixture experiment. Each arm owns its host, repository, worktrees and logs.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import net from 'node:net';
import { fileURLToPath } from 'node:url';
import { spawn, spawnSync } from 'node:child_process';
import { setTimeout as delay } from 'node:timers/promises';

const script = fileURLToPath(import.meta.url);
const fact = 'Invoke the verifier as ./check --portable.';
const args = process.argv.slice(2);
const option = (name, fallback) => args.includes(name) ? args[args.indexOf(name) + 1] : fallback;
const read = file => JSON.parse(fs.readFileSync(file, 'utf8'));
const write = (file, value) => fs.writeFileSync(file, JSON.stringify(value, null, 2) + '\n');
function request(socket, body) {
  return new Promise((resolve, reject) => {
    const connection = net.createConnection(socket);
    let text = '';
    connection.setTimeout(120_000, () => connection.destroy(new Error('Control request timed out')));
    connection.on('error', reject);
    connection.on('connect', () => connection.write(JSON.stringify(body) + '\n'));
    connection.on('data', chunk => {
      text += chunk;
      if (!text.includes('\n')) return;
      connection.end();
      try {
        const response = JSON.parse(text.split('\n')[0]);
        if (!response.ok) throw new Error(response.error);
        resolve(response.result);
      } catch (error) { reject(error); }
    });
  });
}
function run(command, cwd, env = process.env) {
  const result = spawnSync(command[0], command.slice(1), { cwd, env, encoding: 'utf8' });
  if (result.error || result.status !== 0) throw new Error(result.error?.message || result.stderr || result.stdout);
  return result.stdout;
}

// ATOMICS supplies the Claude-style session arguments and the task brief to this wrapper.
// Capture the provider's usage result without estimating tokens from text length.
async function agent() {
  const root = args[1];
  const provider = read(path.join(root, 'provider.json'));
  const supplied = args.slice(2);
  const mcp = JSON.parse(supplied[supplied.indexOf('--mcp-config') + 1]).mcpServers.atomics;
  const pane = mcp.env.ATOMICS_PANE;
  const socket = mcp.env.ATOMICS_SOCKET;
  const brief = supplied.at(-1);
  const isSeed = brief.startsWith('Task seed:');
  const started = performance.now();
  const env = { ...process.env, ...mcp.env, NOTEBOOK_AB_LOG: path.join(root, 'discoveries.jsonl') };
  let usage = null;
  let permissionDenials = [];
  let code = 0;
  if (provider.smoke) {
    let notes = [];
    try { notes = (await request(socket, { cmd: 'notes', pane })).notes; } catch { /* disabled arm */ }
    if (isSeed || !notes.length) run([process.execPath, './discover'], process.cwd(), env);
    run([process.execPath, './check', '--portable'], process.cwd(), env);
    if (isSeed) {
      try { await request(socket, { cmd: 'note', pane, text: fact }); } catch { /* disabled arm */ }
    }
    await request(socket, { cmd: 'report', pane, status: 'done', summary: 'Fixture verifier passed.' });
  } else {
    const log = fs.createWriteStream(path.join(root, `${pane}.jsonl`));
    const child = spawn(provider.command[0], [...provider.command.slice(1), ...supplied], { env, stdio: ['inherit', 'pipe', 'inherit'] });
    let pending = '';
    child.stdout.on('data', chunk => {
      log.write(chunk);
      process.stdout.write(chunk);
      pending += chunk;
      const lines = pending.split('\n');
      pending = lines.pop();
      for (const line of lines) {
        try {
          const event = JSON.parse(line);
          if (event.type === 'result') {
            usage = event.usage ?? null;
            permissionDenials = event.permission_denials ?? [];
          }
        } catch { /* provider diagnostics remain in the raw log */ }
      }
    });
    code = await new Promise((resolve, reject) => { child.on('error', reject); child.on('close', value => resolve(value ?? 1)); });
    await new Promise(resolve => log.end(resolve));
  }
  write(path.join(root, `${pane}.metrics.json`), {
    pane, synthetic: provider.smoke, exit_code: code,
    wall_seconds: (performance.now() - started) / 1000,
    input_tokens: usage?.input_tokens ?? null, output_tokens: usage?.output_tokens ?? null,
    cache_read_input_tokens: usage?.cache_read_input_tokens ?? null,
    cache_creation_input_tokens: usage?.cache_creation_input_tokens ?? null,
    total_tokens: usage ? ['input_tokens', 'output_tokens', 'cache_read_input_tokens', 'cache_creation_input_tokens'].reduce((total, key) => total + (usage[key] ?? 0), 0) : null,
    permission_denials: permissionDenials.length,
  });
  process.exitCode = code;
}

async function arm(root, enabled, provider, binary, timeout) {
  fs.mkdirSync(root, { recursive: true });
  const repo = path.join(root, 'fixture');
  fs.mkdirSync(repo);
  const helper = `#!/usr/bin/env node\nconst fs = require('fs');\nconst record = kind => { if (process.env.NOTEBOOK_AB_LOG) fs.appendFileSync(process.env.NOTEBOOK_AB_LOG, JSON.stringify({pane:process.env.ATOMICS_PANE,at:Date.now(),kind})+'\\n'); };\n`;
  fs.writeFileSync(path.join(repo, 'discover'), helper + `record('discovery'); console.log(${JSON.stringify(fact)});\n`, { mode: 0o755 });
  fs.writeFileSync(path.join(repo, 'check'), helper + `if (!process.argv.includes('--portable')) { record('wrong_invocation'); console.error('Use ./discover to learn the verifier invocation.'); process.exit(1); } record('verified'); console.log('verified');\n`, { mode: 0o755 });
  run(['git', 'init', '-b', 'main'], repo);
  run(['git', 'add', '.'], repo);
  // Per-command fixture identity; never writes any repository or global identity configuration.
  run(['git', '-c', 'user.name=Notebook experiment', '-c', 'user.email=fixture@invalid.invalid', 'commit', '-m', 'Fixture'], repo);
  write(path.join(root, 'provider.json'), provider);
  const config = path.join(root, 'config.toml');
  fs.writeFileSync(config, `base_ref = "main"\nworktree_path = ${JSON.stringify(path.join(root, 'trees/{slug}'))}\n[chief]\nautostart = false\n[plan]\nnotebook = ${enabled}\nclose_done_agents = false\n[cleanup]\ncompleted_plans = false\narchived_plans = false\n[isolation]\nmode = "bare"\n[worktrees]\nwarm = []\n[approvals]\nallow = ["ls", "cat", "rg", "./discover", "./check"]\n[[agents]]\nname = "experiment"\nkind = "claude"\ncommand = ${JSON.stringify([process.execPath, script, '--agent', root])}\n`);
  const socket = path.join(root, 'socket');
  const log = fs.openSync(path.join(root, 'host.log'), 'w');
  const host = spawn(binary, ['host', repo], { env: { ...process.env, ATOMICS_HOME: path.join(root, 'state'), ATOMICS_CONFIG: config, ATOMICS_SOCKET: socket }, stdio: ['ignore', log, log] });
  const ctl = body => request(socket, body);
  const approvals = new Map();
  let cursor = 0;
  const events = [];
  const agents = [];
  let referenceAt;
  const started = performance.now();
  try {
    for (let i = 0; !fs.existsSync(socket); i++) {
      if (host.exitCode !== null || i > 300) throw new Error(`Host failed to start: ${root}/host.log`);
      await delay(100);
    }
    const common = 'Read-only fixture task. Verify the fixture with ./check using the correct invocation. Use ./discover when you need to learn it. Do not edit files or commit. Report done with atomics_report when verified.';
    const plan = (await ctl({ cmd: 'plan_set', goal: 'Verify the same portable fixture in separate fresh agents', tasks: [
      { id: 'seed', title: 'Discover verifier invocation', brief: common + ' First run ./discover. If the notebook is available, propose the discovered invocation with atomics_note.', agent: 'experiment', repo },
      ...['a', 'b'].map(id => ({ id, title: `Verify fixture ${id}`, brief: common, agent: 'experiment', repo })),
    ] })).plan;
    await ctl({ cmd: 'plan_approve', plan });
    for (const task of ['seed', 'a', 'b']) {
      const launch = Date.now();
      await ctl({ cmd: 'plan_dispatch', plan, id: task });
      let metrics;
      while (!metrics) {
        if (Date.now() - launch > timeout * 1000) throw new Error(`Task ${task} timed out; inspect ${root}`);
        const waited = await ctl({ cmd: 'wait', after: cursor, timeout_secs: 1, statuses: ['reported', 'turn done', 'exited', 'needs you', 'plan'] });
        cursor = waited.cursor;
        for (const event of waited.events) {
          events.push(event);
          if (event.approval) approvals.set(event.approval.id, event.session);
        }
        write(path.join(root, 'events.json'), events);
        const view = await ctl({ cmd: 'plan_get', plan, task });
        const pane = view.task.session;
        const file = pane && path.join(root, `${pane}.metrics.json`);
        if (file && fs.existsSync(file)) {
          metrics = read(file);
          if (metrics.exit_code !== 0 || view.task.report?.status !== 'done') throw new Error(`Task ${task} failed or did not report done; inspect ${root}`);
          metrics.task = task;
          metrics.approvals_asked = [...approvals.values()].filter(id => id === pane).length;
          agents.push(metrics);
        }
      }
      if (task === 'seed') {
        if (enabled) {
          // The experiment runner acts as the user: pin one verified proposal through the normal board path.
          const stored = read(path.join(root, 'state/plans', `${plan}.json`));
          const note = stored.notebook?.find(note => note.task === 'seed' && note.text.includes('./check --portable'));
          if (!note) throw new Error('Seed did not propose the verifier fact; the on arm is invalid.');
          await ctl({ cmd: 'curate_note', plan, id: note.id, pinned: true, delete: false });
        }
        referenceAt = Date.now();
      }
      await ctl({ cmd: 'plan_update', plan, id: task, status: 'done', note: 'Experiment runner verified the fixture report and provider exit.' });
    }
    const discoveries = fs.existsSync(path.join(root, 'discoveries.jsonl')) ? fs.readFileSync(path.join(root, 'discoveries.jsonl'), 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse) : [];
    for (const agent of agents) {
      agent.checks_passed = discoveries.filter(e => e.pane === agent.pane && e.kind === 'verified').length;
      if (!agent.checks_passed || agent.task === 'seed' && !discoveries.some(e => e.pane === agent.pane && e.kind === 'discovery')) throw new Error(`Missing verifier/discovery evidence for ${agent.task}; the sample is invalid.`);
      agent.rediscoveries_after_seed = discoveries.filter(e => e.pane === agent.pane && e.kind === 'discovery' && e.at >= referenceAt).length;
      agent.rediscoveries_of_pinned_fact = enabled ? agent.rediscoveries_after_seed : null;
      agent.wrong_invocations = discoveries.filter(e => e.pane === agent.pane && e.kind === 'wrong_invocation').length;
    }
    const result = { notebook: enabled, synthetic: provider.smoke, wall_seconds: (performance.now() - started) / 1000, plan_approvals: 1, pin_actions: enabled ? 1 : 0, approvals_asked: approvals.size, agents };
    write(path.join(root, 'results.json'), result);
    write(path.join(root, 'events.json'), events);
    return result;
  } finally {
    host.kill('SIGTERM');
    await new Promise(resolve => { if (host.exitCode !== null) resolve(); else host.once('exit', resolve); });
    fs.closeSync(log);
  }
}

async function main() {
  if (args.includes('--help')) {
    console.log('Usage: atomics queue --class qa -- node scripts/notebook-ab.mjs --atomics /path/to/atomics [--pairs 3] [--output DIR] [--command-json JSON_ARRAY] [--timeout 900] [--smoke]\nDefault provider: claude --print --verbose --output-format stream-json. --smoke uses synthetic agents and reports null tokens.');
    return;
  }
  const root = path.resolve(option('--output', null) || fs.mkdtempSync(path.join(os.tmpdir(), 'notebook-ab-')));
  const binary = path.resolve(option('--atomics', 'target/debug/atomics'));
  const pairs = Number(option('--pairs', '1'));
  const timeout = Number(option('--timeout', '900'));
  if (!Number.isInteger(pairs) || pairs < 1 || !Number.isFinite(timeout) || timeout <= 0) throw new Error('Use positive --pairs and --timeout values.');
  const provider = { smoke: args.includes('--smoke'), command: JSON.parse(option('--command-json', '["claude","--print","--verbose","--output-format","stream-json"]')) };
  if (fs.existsSync(root) && fs.readdirSync(root).length) throw new Error('Use a fresh, empty output directory.');
  fs.mkdirSync(root, { recursive: true });
  write(path.join(root, 'manifest.json'), { atomics_version: run([binary, '--version']), node_version: process.version, provider, pairs, timeout, fact, started_at: new Date().toISOString() });
  console.log(`Artifacts: ${root}; synthetic: ${provider.smoke}`);
  const results = [];
  for (let pair = 0; pair < pairs; pair++) {
    for (const enabled of pair % 2 ? [true, false] : [false, true]) {
      const result = await arm(path.join(root, `${pair}-${enabled ? 'on' : 'off'}`), enabled, provider, binary, timeout);
      results.push({ pair, ...result });
      console.log(JSON.stringify(results.at(-1)));
    }
  }
  write(path.join(root, 'results.json'), results);
  if (provider.smoke) {
    for (const result of results) {
      if (result.agents.some(a => a.input_tokens !== null)) throw new Error('Synthetic tokens must remain null.');
      const peers = result.agents.filter(a => ['a', 'b'].includes(a.task));
      if (peers.some(a => a.rediscoveries_after_seed !== (result.notebook ? 0 : 1))) throw new Error('Smoke evidence did not match expected pull behavior.');
    }
  }
}
try { if (args[0] === '--agent') await agent(); else await main(); }
catch (error) { console.error(error); process.exitCode = 1; }
