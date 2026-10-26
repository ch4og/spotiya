// The slice of a browser the BotGuard VM touches, shaped after jsdom (which Google accepts).
// DOM_TABLE (dumped from jsdom) supplies every member name per prototype level; the VM
// probes random names a few levels up each chain, so the chain shape has to match too.
(function (T) {
  var SEEN = {};
  function trace(name, target) {
    if (typeof Proxy === 'undefined' || !globalThis.BG_TRACE) return target;
    return new Proxy(target, {
      get: function (t, k) {
        if (typeof k === 'string' && !(k in t) && !SEEN[name + '.' + k]) { SEEN[name + '.' + k] = 1; log('miss ' + name + '.' + k); }
        return t[k];
      }
    });
  }
  function stub() { return null; }
  function value(v) { return v === 1 ? stub : v === 2 ? {} : v === null ? null : v[0]; }
  function fill(target, props) { for (var k in props) if (!Object.prototype.hasOwnProperty.call(target, k)) target[k] = value(props[k]); }
  function define(proto, name, get, set) { Object.defineProperty(proto, name, { get: get, set: set, configurable: true, enumerable: true }); }

  var P = {};
  function proto(name) {
    if (P[name]) return P[name];
    var spec = T.protos[name], p = Object.create(spec.parent ? proto(spec.parent) : Object.prototype);
    fill(p, spec.props);
    Object.defineProperty(p, 'constructor', { value: function () {}, configurable: true, writable: true });
    Object.defineProperty(p.constructor, 'name', { value: name });
    if (typeof Symbol !== 'undefined') Object.defineProperty(p, Symbol.toStringTag, { value: name, configurable: true });
    return (P[name] = p);
  }
  for (var n in T.protos) proto(n);

  var t0 = Date.now();
  if (!globalThis.performance) globalThis.performance = { timeOrigin: t0, now: function () { return Date.now() - t0; } };

  // Elements keep their state off-instance, like jsdom's impl objects.
  var S = new WeakMap();
  function st(e) { return S.get(e) || {}; }
  function element(tag) {
    tag = String(tag).toLowerCase();
    var o = T.objects[tag], e = Object.create(o ? P[o.proto] : P.HTMLElement);
    var state = { tag: tag, attrs: {}, children: [], style: Object.create(P.CSSStyleProperties), text: '', parent: null, win: null };
    S.set(e, state);
    var t = trace('<' + tag + '>', e);
    S.set(t, state);
    return t;
  }
  var EL = P.Element, NODE = P.Node, ET = P.EventTarget;
  define(EL, 'tagName', function () { return st(this).tag.toUpperCase(); });
  define(NODE, 'nodeName', function () { return S.has(this) ? st(this).tag.toUpperCase() : '#document'; });
  define(EL, 'localName', function () { return st(this).tag; });
  define(EL, 'id', function () { return st(this).attrs.id || ''; }, function (v) { st(this).attrs.id = String(v); });
  define(EL, 'className', function () { return st(this).attrs['class'] || ''; }, function (v) { st(this).attrs['class'] = String(v); });
  define(P.HTMLElement, 'style', function () { return st(this).style; });
  define(NODE, 'textContent', function () { return st(this).text || ''; }, function (v) { st(this).text = String(v); });
  define(NODE, 'parentNode', function () { return st(this).parent || null; });
  define(NODE, 'parentElement', function () { return st(this).parent || null; });
  define(NODE, 'childNodes', function () { return st(this).children; });
  define(EL, 'children', function () { return st(this).children; });
  define(NODE, 'firstChild', function () { return st(this).children[0] || null; });
  define(NODE, 'isConnected', function () { return !!st(this).parent; });
  define(P.HTMLIFrameElement, 'contentWindow', function () { return st(this).win; });
  define(P.HTMLIFrameElement, 'contentDocument', function () { return st(this).win && st(this).win.document; });
  NODE.appendChild = function (c) {
    st(this).children.push(c);
    var s = S.get(c);
    if (s) { s.parent = this; if (s.tag === 'iframe' && !s.win) s.win = frameWindow(); }
    return c;
  };
  NODE.insertBefore = NODE.appendChild;
  NODE.removeChild = function (c) {
    var ch = st(this).children, i = ch.indexOf(c);
    if (i >= 0) ch.splice(i, 1);
    var s = S.get(c);
    if (s) { s.parent = null; s.win = null; }
    return c;
  };
  NODE.contains = function (c) { return st(this).children.indexOf(c) >= 0; };
  NODE.hasChildNodes = function () { return st(this).children.length > 0; };
  NODE.lookupNamespaceURI = function () { return null; };
  NODE.isSameNode = function (o) { return o === this; };
  EL.setAttribute = function (k, v) { st(this).attrs[String(k).toLowerCase()] = String(v); };
  EL.getAttribute = function (k) { var a = st(this).attrs; k = String(k).toLowerCase(); return k in a ? a[k] : null; };
  EL.hasAttribute = function (k) { return String(k).toLowerCase() in st(this).attrs; };
  EL.removeAttribute = function (k) { delete st(this).attrs[String(k).toLowerCase()]; };
  EL.getAttributeNames = function () { return Object.keys(st(this).attrs); };
  EL.hasAttributes = function () { return Object.keys(st(this).attrs).length > 0; };
  EL.getElementsByTagName = function () { return []; };
  EL.querySelector = function () { return null; };
  EL.querySelectorAll = function () { return []; };
  EL.getBoundingClientRect = function () { return { x: 0, y: 0, width: 0, height: 0, top: 0, left: 0, right: 0, bottom: 0 }; };
  EL.getClientRects = function () { return []; };
  ET.addEventListener = function () {};
  ET.removeEventListener = function () {};
  ET.dispatchEvent = function () { return true; };
  P.HTMLCanvasElement.getContext = function () { return null; };

  function Event(type) { this.type = type; this.bubbles = false; this.cancelable = false; this.isTrusted = false; this.timeStamp = performance.now(); }
  Event.prototype.initEvent = function (type, bubbles, cancelable) { this.type = type; this.bubbles = !!bubbles; this.cancelable = !!cancelable; };
  Event.prototype.initMouseEvent = Event.prototype.initEvent;
  Event.prototype.preventDefault = function () {};
  Event.prototype.stopPropagation = function () {};

  var loc = Object.create(P.Location);
  fill(loc, T.objects.location.own);
  loc.toString = loc.valueOf = function () { return loc.href; };
  globalThis.location = trace('location', loc);

  var doc = Object.create(P.Document);
  var html = element('html'), head = element('head'), body = element('body');
  html.setAttribute('lang', 'en');
  html.appendChild(head);
  html.appendChild(body);
  doc.location = globalThis.location;
  var D = P.Document;
  define(D, 'documentElement', function () { return html; });
  define(D, 'head', function () { return head; });
  define(D, 'body', function () { return body; });
  define(D, 'defaultView', function () { return globalThis.window; });
  define(D, 'URL', function () { return loc.href; });
  define(D, 'documentURI', function () { return loc.href; });
  define(D, 'readyState', function () { return 'complete'; });
  define(D, 'hidden', function () { return true; });
  define(D, 'visibilityState', function () { return 'hidden'; });
  define(D, 'cookie', function () { return ''; }, function () {});
  define(D, 'referrer', function () { return 'https://www.youtube.com/'; });
  D.createElement = function (t) { return element(t); };
  D.createElementNS = function (ns, t) { return element(t); };
  D.createEvent = function () { return new Event(''); };
  D.getElementById = function () { return null; };
  D.getElementsByTagName = function (t) { t = String(t).toLowerCase(); return t === 'body' ? [body] : t === 'head' ? [head] : t === 'html' ? [html] : []; };
  D.getElementsByClassName = function () { return []; };
  D.querySelector = function () { return null; };
  D.querySelectorAll = function () { return []; };
  D.hasFocus = function () { return false; };
  globalThis.document = trace('document', doc);

  var nav = Object.create(P.Navigator);
  define(P.Navigator, 'languages', function () { return ['en-US', 'en']; });
  define(P.Navigator, 'plugins', function () { return { length: 0 }; });
  define(P.Navigator, 'mimeTypes', function () { return { length: 0 }; });
  if (!globalThis.navigator || !globalThis.navigator.userAgent || /node/i.test(globalThis.navigator.userAgent))
    Object.defineProperty(globalThis, 'navigator', { value: trace('navigator', nav), configurable: true, writable: true });
  globalThis.screen = Object.create(P.Screen);
  globalThis.history = Object.create(P.History);

  // jsdom gives an attached iframe an about:blank window sharing the outer realm's builtins.
  function frameWindow() {
    var w = Object.create(P.Window), fdoc = Object.create(P.Document);
    Object.getOwnPropertyNames(globalThis).forEach(function (k) { try { w[k] = globalThis[k]; } catch (e) {} });
    w.window = w.self = w.frames = w;
    w.top = w.parent = globalThis;
    w.document = fdoc;
    return w;
  }

  var own = T.objects.window.own;
  for (var k in own) {
    if (k in globalThis) continue;
    try { globalThis[k] = value(own[k]); } catch (e) {}
  }
  globalThis.self = globalThis.top = globalThis.parent = globalThis.frames = globalThis;
  globalThis.origin = 'https://www.youtube.com';
  globalThis.addEventListener = ET.addEventListener;
  globalThis.removeEventListener = ET.removeEventListener;
  globalThis.dispatchEvent = ET.dispatchEvent;
  try { Object.setPrototypeOf(globalThis, P.Window); } catch (e) {}
  globalThis.window = trace('window', globalThis);
})(globalThis.DOM_TABLE);
