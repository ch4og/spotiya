// Host glue for running BotGuard and minting WebPO tokens. Rust drives it: it calls
// one of the __bg* entry points, then pumps jobs and __runTimers() until __out.done.
var __timers = [], __timerSeq = 0;
globalThis.setTimeout = function (f, ms) {
  var args = [].slice.call(arguments, 2);
  __timers.push({ id: ++__timerSeq, at: Date.now() + (+ms || 0), f: f, args: args });
  return __timerSeq;
};
globalThis.clearTimeout = function (id) { __timers = __timers.filter(function (t) { return t.id !== id; }); };
globalThis.setInterval = function (f, ms) {
  var box = { id: 0 };
  (function tick() { box.id = setTimeout(function () { tick(); f(); }, ms); })();
  return box;
};
globalThis.clearInterval = function (box) { if (box) clearTimeout(box.id); };
// Runs due timers; returns ms until the next one, or -1 when none are queued.
function __runTimers() {
  var now = Date.now(), due = __timers.filter(function (t) { return t.at <= now; });
  __timers = __timers.filter(function (t) { return t.at > now; });
  due.forEach(function (t) { try { typeof t.f === 'function' ? t.f.apply(null, t.args) : (0, eval)(String(t.f)); } catch (e) {} });
  if (!__timers.length) return -1;
  return Math.max(0, Math.min.apply(null, __timers.map(function (t) { return t.at; })) - Date.now());
}

var B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
if (typeof btoa === 'undefined') globalThis.btoa = function (s) {
  var o = '';
  for (var i = 0; i < s.length; i += 3) {
    var a = s.charCodeAt(i), b = s.charCodeAt(i + 1), c = s.charCodeAt(i + 2);
    var n = (a << 16) | ((b || 0) << 8) | (c || 0);
    o += B64[n >> 18 & 63] + B64[n >> 12 & 63] + (i + 1 < s.length ? B64[n >> 6 & 63] : '=') + (i + 2 < s.length ? B64[n & 63] : '=');
  }
  return o;
};
if (typeof atob === 'undefined') globalThis.atob = function (s) {
  s = s.replace(/[^A-Za-z0-9+/]/g, '');
  var o = '', bits = 0, acc = 0;
  for (var i = 0; i < s.length; i++) { acc = (acc << 6) | B64.indexOf(s[i]); bits += 6; if (bits >= 8) { bits -= 8; o += String.fromCharCode(acc >> bits & 255); } }
  return o;
};
if (typeof TextEncoder === 'undefined') {
  globalThis.TextEncoder = function () {};
  TextEncoder.prototype.encode = function (s) { var u = unescape(encodeURIComponent(s)), a = new Uint8Array(u.length); for (var i = 0; i < u.length; i++) a[i] = u.charCodeAt(i); return a; };
  globalThis.TextDecoder = function () {};
  TextDecoder.prototype.decode = function (a) { var s = ''; for (var i = 0; i < a.length; i++) s += String.fromCharCode(a[i]); return decodeURIComponent(escape(s)); };
}
function log() {}

function __b64ToU8(s) {
  var bin = atob(s.replace(/-/g, '+').replace(/_/g, '/').replace(/\./g, '=')), u = new Uint8Array(bin.length);
  for (var i = 0; i < bin.length; i++) u[i] = bin.charCodeAt(i);
  return u;
}
function __u8ToB64url(u) {
  var s = '';
  for (var i = 0; i < u.length; i++) s += String.fromCharCode(u[i]);
  return btoa(s).replace(/\+/g, '-').replace(/\//g, '_');
}

var __out = { done: true, value: null, error: null };
function __settle(p) {
  __out = { done: false, value: null, error: null };
  Promise.resolve(p).then(function (v) { __out.value = v; __out.done = true; },
    function (e) { __out.error = String(e && e.stack || e); __out.done = true; });
}

var __signals = null, __minter = null;
// Loads the VM and takes the snapshot whose response GenerateIT trades for an integrity token.
function __bgStart(interpreter, program, globalName, ytcfg) {
  __settle(new Promise(function (resolve, reject) {
    globalThis.yt = { config_: JSON.parse(ytcfg) };
    new Function(interpreter)();
    var vm = globalThis[globalName];
    if (!vm || !vm.a) throw new Error('BotGuard VM missing');
    __signals = [];
    vm.a(program, function (snapshot) {
      snapshot(resolve, [undefined, undefined, __signals, undefined]);
    }, true, undefined, function () {}, [[], []]);
  }));
}
function __bgMinter(integrityToken) {
  __settle((async function () {
    if (!__signals || !__signals[0]) throw new Error('BotGuard gave no minter');
    __minter = await __signals[0](__b64ToU8(integrityToken));
    if (typeof __minter !== 'function') throw new Error('minter is not a function');
    return 'ok';
  })());
}
function __bgMint(binding) {
  __settle((async function () { return __u8ToB64url(await __minter(new TextEncoder().encode(binding))); })());
}
