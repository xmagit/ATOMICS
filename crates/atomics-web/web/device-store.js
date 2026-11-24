// This browser's Noise static key for each host it paired with (docs/daemon.md, "The browser client
// and its embedding").
//
// IndexedDB keeps one record per host: the host's public key and the device key sealed with AES-GCM
// (the host id and key are its additional data). The sealing key is a WebCrypto key made
// non-extractable, so no script can read it out, and a copy of the database alone is ciphertext.
// Script on this origin can still use the sealing key, which it must to run a handshake: that is the
// limit of keeping a key in a browser, and the
// reason the client runs on an origin of its own. Nothing here logs a key.
//
// What storage did is kept as facts (`storageFacts`) for the pairing screens and the host's pairing
// prompt: how the database opened, what the last read and save found, and when this browser last saved
// or forgot a host's device, marked in localStorage so a lost database can be told from a save that never
// happened. Names, counts, outcomes and times only: never a key, a ciphertext or anything made from one.

const DB = 'atomics-devices';
const KEYS = 'keys';
const DEVICES = 'devices';
const SEAL = 'seal';
const MARKS = 'atomics-device-marks';

const facts = { open: 'not-run', seal: 'unknown', records: null, read: 'not-run', save: 'not-run', persist: null };
// The database was made during this page's life: it did not exist when the page first opened it.
let created = false;
const errorName = error => (typeof error?.name === 'string' && /^[A-Za-z]{1,40}$/.test(error.name) ? error.name : 'Error');

let opened;
function discard(connection) {
  if (opened === connection) opened = undefined;
}

function open() {
  if (!opened) {
    const connection = new Promise((resolve, reject) => {
      let request;
      try {
        request = indexedDB.open(DB, 1);
      } catch (error) {
        facts.open = `failed ${errorName(error)}`;
        throw error;
      }
      request.onupgradeneeded = event => {
        created ||= event.oldVersion === 0;
        request.result.createObjectStore(KEYS);
        request.result.createObjectStore(DEVICES);
      };
      request.onblocked = () => { facts.open = 'blocked'; };
      request.onsuccess = () => {
        facts.open = created ? 'new' : 'existing';
        const db = request.result;
        // A backgrounded browser may close the connection. A schema change must also let it go.
        db.onclose = () => discard(connection);
        db.onversionchange = () => { db.close(); discard(connection); };
        resolve(db);
      };
      request.onerror = () => {
        facts.open = `failed ${errorName(request.error)}`;
        reject(request.error);
      };
    });
    opened = connection;
    // An unsuccessful open must not poison every later attempt, including Try again.
    connection.catch(() => discard(connection));
  }
  return opened;
}

async function withDatabase(action) {
  for (let attempt = 0; ; attempt++) {
    const connection = open();
    let db;
    try {
      db = await connection;
      return await action(db);
    } catch (error) {
      // A close can precede its event, or interrupt a transaction. Retry once with a new connection.
      // Quota, permission, malformed records and decrypt errors stay errors, never "no device".
      const lost = ['InvalidStateError', 'TransactionInactiveError', 'AbortError', 'UnknownError'].includes(error?.name);
      if (!lost) throw error;
      db?.close();
      discard(connection);
      if (attempt) throw error;
    }
  }
}

function transact(db, name, mode, action) {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(name, mode);
    const request = action(tx.objectStore(name));
    let result;
    request.onsuccess = () => { result = request.result; };
    request.onerror = () => reject(request.error);
    // A request's success is not a committed write: an interrupted transaction may still abort.
    tx.oncomplete = () => resolve(result);
    tx.onabort = () => reject(tx.error ?? new DOMException('Storage transaction interrupted', 'AbortError'));
  });
}

const aad = (hostId, hostKey) => new TextEncoder().encode(`atomics-device-v1\n${hostId}\n${hostKey}`);

// What the database holds, for the facts: whether a sealing key is kept and how many devices. Counting
// reads no value, so it unwraps no key. It never fails the operation it follows.
async function survey(db) {
  try {
    facts.seal = (await transact(db, KEYS, 'readonly', s => s.count(SEAL))) ? 'present' : 'absent';
    facts.records = await transact(db, DEVICES, 'readonly', s => s.count());
  } catch (error) {
    facts.seal = `unknown ${errorName(error)}`;
    facts.records = null;
  }
}

// When this browser last saved or forgot `hostId`'s device: `{saved: time}` or `{forgot: time}`.
function mark(hostId, kind) {
  try {
    const marks = JSON.parse(localStorage.getItem(MARKS) || '{}');
    marks[hostId] = { [kind]: new Date().toISOString() };
    localStorage.setItem(MARKS, JSON.stringify(marks));
  } catch {
    // A browser that keeps nothing here: the facts say so when they are read.
  }
}

function marked(hostId) {
  try {
    const kept = JSON.parse(localStorage.getItem(MARKS) || '{}')[hostId];
    if (typeof kept?.saved === 'string') return `saved ${kept.saved}`;
    if (typeof kept?.forgot === 'string') return `forgot ${kept.forgot}`;
    return 'none';
  } catch (error) {
    return `unreadable ${errorName(error)}`;
  }
}

async function ask(question) {
  try {
    const answer = await question();
    return typeof answer === 'boolean' ? answer : null;
  } catch (error) {
    return `failed ${errorName(error)}`;
  }
}

// What this page knows about its storage for `hostId`. `persisted` and `access` are null where the
// browser does not have the question; `standalone` is iOS's installed-app flag.
export async function storageFacts(hostId) {
  return {
    ...facts,
    mark: marked(hostId),
    persisted: await ask(() => navigator.storage?.persisted?.()),
    access: await ask(() => document.hasStorageAccess?.()),
    standalone: typeof navigator.standalone === 'boolean' ? navigator.standalone : null,
  };
}

async function sealingKey(db) {
  const kept = await transact(db, KEYS, 'readonly', s => s.get(SEAL));
  if (kept) {
    facts.seal = 'present';
    return kept;
  }
  const key = await crypto.subtle.generateKey({ name: 'AES-GCM', length: 256 }, false, ['encrypt', 'decrypt']);
  try {
    await transact(db, KEYS, 'readwrite', s => s.add(key, SEAL));
    facts.seal = 'made';
    return key;
  } catch (error) {
    if (error?.name !== 'ConstraintError') throw error;
    // Another tab made one first: use that.
    const kept = await transact(db, KEYS, 'readonly', s => s.get(SEAL));
    if (!kept) throw new Error('The storage sealing key could not be read');
    return kept;
  }
}

// Best effort, at every boot as well as after saving. A denied request is shown to the user; it must
// neither block a stored key's use nor turn a committed save into a failure. null means unsupported.
export async function requestPersistence() {
  try {
    if (!navigator.storage?.persist) return (facts.persist = null);
    return (facts.persist = await navigator.storage.persist());
  } catch {
    return (facts.persist = false);
  }
}

// Replaces what was kept for the host only once the host accepted a pairing. The driver reuses an
// existing device key and asks first when the pairing names a different host key (ConfirmRepair).
//
// It resolves once the write committed and a new transaction reads it back; the facts name the step that
// failed otherwise (seal, encrypt, write or check).
export async function saveDevice(hostId, hostKey, secret) {
  // The caller's bytes are a view of wasm memory: copy them before anything waits.
  const plain = Uint8Array.from(secret);
  const pairedAt = Date.now();
  let step = 'open';
  facts.save = 'started';
  try {
    await withDatabase(async db => {
      step = 'seal';
      const key = await sealingKey(db);
      step = 'encrypt';
      const iv = crypto.getRandomValues(new Uint8Array(12));
      const sealed = await crypto.subtle.encrypt({ name: 'AES-GCM', iv, additionalData: aad(hostId, hostKey) }, key, plain);
      step = 'write';
      await transact(db, DEVICES, 'readwrite', s => s.put({ v: 1, hostKey, iv, sealed, pairedAt }, hostId));
    });
    step = 'check';
    const back = await withDatabase(db => transact(db, DEVICES, 'readonly', s => s.get(hostId)));
    if (back?.pairedAt !== pairedAt) throw new DOMException('The saved device was not read back', 'NotFoundError');
    facts.save = 'confirmed';
    mark(hostId, 'saved');
  } catch (error) {
    facts.save = `failed ${step} ${errorName(error)}`;
    throw error;
  } finally {
    plain.fill(0);
  }
}

export async function loadDevice(hostId) {
  facts.read = 'started';
  try {
    const device = await readDevice(hostId);
    facts.read = device ? 'found' : 'none';
    return device;
  } catch (error) {
    facts.read = `failed ${errorName(error)}`;
    throw error;
  }
}

function readDevice(hostId) {
  return withDatabase(async db => {
    const record = await transact(db, DEVICES, 'readonly', s => s.get(hostId));
    await survey(db);
    if (record === undefined) return null;
    if (record?.v !== 1) throw new Error('The stored device could not be read');
    const key = await transact(db, KEYS, 'readonly', s => s.get(SEAL));
    if (!key) throw new Error('The storage sealing key could not be read');
    const plain = await crypto.subtle.decrypt(
      { name: 'AES-GCM', iv: record.iv, additionalData: aad(hostId, record.hostKey) }, key, record.sealed);
    return { hostKey: record.hostKey, secret: new Uint8Array(plain) };
  });
}

export async function forgetDevice(hostId) {
  await withDatabase(db => transact(db, DEVICES, 'readwrite', s => s.delete(hostId)));
  mark(hostId, 'forgot');
}
