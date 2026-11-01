// Browser-fingerprint hardening shim, injected via
// Page.addScriptToEvaluateOnNewDocument before any page script runs.
//
// Two rules drive every patch below:
//   1. Patch only what headless Chrome actually gets wrong. Anything the
//      browser already reports truthfully is left alone — an out-of-spec
//      constant scores worse than a missing patch.
//   2. A patched callable is worthless if Function.prototype.toString
//      prints its JavaScript source. Every replacement is wrapped so the
//      toString probe sees a native-looking body instead.

(() => {
  'use strict';

  // ── toString disguise ──────────────────────────────────────────────
  // Function.prototype.toString is the first thing fingerprint code runs
  // on a suspicious function. Proxy it so callables registered below
  // report `function <name>() { [native code] }` like the real ones.
  const nativeSource = new Map();
  const realToString = Function.prototype.toString;
  const asNative = (fn, name) => {
    nativeSource.set(fn, `function ${name}() { [native code] }`);
    return fn;
  };
  Function.prototype.toString = asNative(
    new Proxy(realToString, {
      apply(target, thisArg, args) {
        const forged = nativeSource.get(thisArg);
        return forged === undefined
          ? Reflect.apply(target, thisArg, args)
          : forged;
      },
    }),
    'toString',
  );

  // WebIDL accessors/methods are enumerable + configurable; match that
  // when installing replacements so property descriptors stay identical.
  const defineGetter = (obj, prop, get) => {
    Object.defineProperty(obj, prop, {
      get: asNative(get, `get ${prop}`),
      enumerable: true,
      configurable: true,
    });
  };
  const defineMethod = (obj, prop, fn) => {
    Object.defineProperty(obj, prop, {
      value: asNative(fn, prop),
      writable: true,
      enumerable: true,
      configurable: true,
    });
  };
  const ownerOf = (obj, prop) => {
    for (let o = obj; o; o = Object.getPrototypeOf(o)) {
      if (Object.prototype.hasOwnProperty.call(o, prop)) return o;
    }
    return obj;
  };
  const defineGetters = (obj, values) => {
    for (const [prop, value] of Object.entries(values)) {
      Object.defineProperty(obj, prop, {
        get: asNative(() => value, `get ${prop}`),
        enumerable: true,
        configurable: true,
      });
    }
  };

  const IS_MAC =
    navigator.platform === 'MacIntel' ||
    navigator.userAgentData?.platform === 'macOS';
  const IS_WIN =
    (navigator.platform || '').startsWith('Win') ||
    navigator.userAgentData?.platform === 'Windows';
  // Height of the browser chrome (tab strip + toolbar + menubar inset is
  // accounted separately). Measured on shipping Chrome: macOS 87, Win 79.
  const CHROME_UI_HEIGHT = IS_MAC ? 87 : IS_WIN ? 79 : 87;

  // ── navigator.webdriver ────────────────────────────────────────────
  // Real non-automated Chrome reports `false` (the attribute exists), so
  // report false rather than deleting it — `undefined` is itself a tell.
  if (navigator.webdriver !== false) {
    defineGetter(Navigator.prototype, 'webdriver', () => false);
  }

  // ── navigator.languages / language ─────────────────────────────────
  // Headless reports a lone primary tag; real Chrome appends the base
  // subtag ("en-US" -> ["en-US", "en"]). Only normalize short lists.
  try {
    const langs = Array.from(navigator.languages || []);
    if (langs.length <= 1) {
      const primary = navigator.language || langs[0] || 'en-US';
      const base = primary.split('-')[0];
      const patched = primary === base ? [primary] : [primary, base];
      defineGetter(Navigator.prototype, 'languages', () => patched.slice());
      defineGetter(Navigator.prototype, 'language', () => primary);
    }
  } catch (_e) {}

  // ── window.chrome.runtime ──────────────────────────────────────────
  // Real Chrome exposes chrome.{app,csi,loadTimes,runtime} to every page.
  // Headless provides the first three natively; only fill what is absent.
  try {
    const chromeObj =
      typeof window.chrome === 'object' && window.chrome !== null
        ? window.chrome
        : undefined;
    const target = chromeObj ?? {};
    if (typeof target.runtime === 'undefined') {
      const stubEvent = () => ({
        addListener: asNative(() => {}, 'addListener'),
        removeListener: asNative(() => {}, 'removeListener'),
        hasListener: asNative(() => false, 'hasListener'),
      });
      target.runtime = {
        OnInstalledReason: { CHROME_UPDATE: 'chrome_update' },
        OnRestartRequiredReason: { APP_UPDATE: 'app_update' },
        PlatformArch: {
          ARM: 'arm',
          ARM64: 'arm64',
          X86_32: 'x86-32',
          X86_64: 'x86-64',
        },
        PlatformNaclArch: {
          ARM: 'arm',
          X86_32: 'x86-32',
          X86_64: 'x86-64',
        },
        PlatformOs: {
          ANDROID: 'android',
          CROS: 'cros',
          LINUX: 'linux',
          MAC: 'mac',
          OPENBSD: 'openbsd',
          WIN: 'win',
        },
        RequestUpdateCheckStatus: {
          NO_UPDATE: 'no_update',
          THROTTLED: 'throttled',
          UPDATE_AVAILABLE: 'update_available',
        },
        connect: asNative(() => {
          throw new TypeError(
            'Error in invocation of runtime.connect(): No matching signature.',
          );
        }, 'connect'),
        sendMessage: asNative(() => {
          throw new TypeError(
            'Error in invocation of runtime.sendMessage(): No matching signature.',
          );
        }, 'sendMessage'),
        onConnect: stubEvent(),
        onMessage: stubEvent(),
      };
    }
    if (typeof target.app === 'undefined') {
      target.app = {
        isInstalled: false,
        InstallState: { DISABLED: 'disabled', INSTALLED: 'installed', NOT_INSTALLED: 'not_installed' },
        RunningState: { CANNOT_RUN: 'cannot_run', READY_TO_RUN: 'ready_to_run', RUNNING: 'running' },
      };
    }
    if (typeof target.csi !== 'function') {
      target.csi = asNative(() => ({ onloadT: 0, pageT: 0, startE: 0, tran: 15 }), 'csi');
    }
    if (typeof target.loadTimes !== 'function') {
      target.loadTimes = asNative(
        () => ({
          commitLoadTime: 0,
          connectionInfo: 'h2',
          finishDocumentLoadTime: 0,
          finishLoadTime: 0,
          firstPaintAfterLoadTime: 0,
          firstPaintTime: 0,
          navigationType: 'Other',
          npnNegotiatedProtocol: 'h2',
          requestTime: 0,
          startLoadTime: 0,
          wasAlternateProtocolAvailable: false,
          wasFetchedViaSpdy: true,
          wasNpnNegotiated: true,
        }),
        'loadTimes',
      );
    }
    if (!chromeObj) {
      Object.defineProperty(window, 'chrome', {
        value: target,
        writable: true,
        enumerable: true,
        configurable: true,
      });
    }
  } catch (_e) {}

  // ── navigator.permissions.query ────────────────────────────────────
  // Headless answers "denied" for the notifications query while a real
  // browser answers "default". Patch the resolved PermissionStatus in
  // place (state is a prototype getter) so the object keeps its real
  // type and event surface — a fabricated plain object fails instanceof.
  try {
    const permissions = navigator.permissions;
    if (permissions && typeof permissions.query === 'function') {
      const proto = ownerOf(permissions, 'query');
      const realQuery = permissions.query.bind(permissions);
      defineMethod(proto, 'query', function query(parameters) {
        return realQuery(parameters).then((result) => {
          if (
            parameters &&
            parameters.name === 'notifications' &&
            result &&
            result.state === 'denied'
          ) {
            // PermissionStatus.state uses 'prompt', unlike
            // Notification.permission which spells it 'default'.
            Object.defineProperty(result, 'state', {
              value: 'prompt',
              writable: true,
              enumerable: true,
              configurable: true,
            });
          }
          return result;
        });
      });
    }
  } catch (_e) {}

  // ── navigator.plugins / mimeTypes ──────────────────────────────────
  // Only fires when the browser has an empty plugin list (old headless).
  // Instances are built on the real prototypes so instanceof, the proto
  // methods and Object.prototype.toString all behave like real objects.
  try {
    if (navigator.plugins && navigator.plugins.length === 0) {
      const mkMimeType = (type, suffixes, description, enabledPlugin) => {
        const mt = Object.create(MimeType.prototype);
        defineGetters(mt, { type, suffixes, description, enabledPlugin });
        return mt;
      };
      const mkPlugin = (name, filename, description, mimeTypes) => {
        const plugin = Object.create(Plugin.prototype);
        defineGetters(plugin, { name, filename, description });
        mimeTypes.forEach((mt, i) => {
          Object.defineProperty(plugin, i, { value: mt, enumerable: true, configurable: true });
          Object.defineProperty(plugin, mt.type, { value: mt, enumerable: false, configurable: true });
        });
        defineGetter(plugin, 'length', () => mimeTypes.length);
        return plugin;
      };
      const mkPluginArray = (plugins) => {
        const arr = Object.create(PluginArray.prototype);
        plugins.forEach((plugin, i) => {
          Object.defineProperty(arr, i, { value: plugin, enumerable: true, configurable: true });
          Object.defineProperty(arr, plugin.name, {
            value: plugin,
            enumerable: false,
            configurable: true,
          });
        });
        defineGetter(arr, 'length', () => plugins.length);
        defineMethod(arr, 'item', (i) => plugins[i] ?? null);
        defineMethod(arr, 'namedItem', (n) => plugins.find((p) => p.name === n) ?? null);
        defineMethod(arr, 'refresh', () => {});
        return arr;
      };
      const mkMimeTypeArray = (mimeTypes) => {
        const arr = Object.create(MimeTypeArray.prototype);
        mimeTypes.forEach((mt, i) => {
          Object.defineProperty(arr, i, { value: mt, enumerable: true, configurable: true });
          Object.defineProperty(arr, mt.type, { value: mt, enumerable: false, configurable: true });
        });
        defineGetter(arr, 'length', () => mimeTypes.length);
        defineMethod(arr, 'item', (i) => mimeTypes[i] ?? null);
        defineMethod(arr, 'namedItem', (n) => mimeTypes.find((m) => m.type === n) ?? null);
        return arr;
      };
      const pdfMime = mkMimeType('application/pdf', 'pdf', 'Portable Document Format', null);
      const pluginDefs = [
        ['PDF Viewer', 'internal-pdf-viewer', 'Portable Document Format'],
        ['Chrome PDF Viewer', 'internal-pdf-viewer', ''],
        ['Chromium PDF Viewer', 'internal-pdf-viewer', ''],
        ['Chromium PDF Plugin', 'internal-pdf-viewer', 'Portable Document Format'],
        ['Microsoft Edge PDF Viewer', 'internal-pdf-viewer', 'Portable Document Format'],
      ];
      const pdfPlugins = pluginDefs
        .slice(0, 5)
        .map(([name, filename, description]) =>
          mkPlugin(name, filename, description, [pdfMime]),
        );
      Object.defineProperty(pdfMime, 'enabledPlugin', {
        get: asNative(() => pdfPlugins[0], 'get enabledPlugin'),
        enumerable: true,
        configurable: true,
      });
      // navigator.plugins/mimeTypes return a stable identity across reads.
      const pluginsArr = mkPluginArray(pdfPlugins);
      const mimeTypesArr = mkMimeTypeArray([pdfMime]);
      defineGetter(Navigator.prototype, 'plugins', () => pluginsArr);
      defineGetter(Navigator.prototype, 'mimeTypes', () => mimeTypesArr);
    }
  } catch (_e) {}

  // ── WebGL vendor/renderer ──────────────────────────────────────────
  // Headless already reports the real GPU through ANGLE; only rewrite
  // software rasterizers (SwiftShader/llvmpipe), which no real user GPU
  // produces. Values are platform-plausible: Intel-on-Apple-Silicon is
  // itself a contradiction, so pick per-OS strings.
  try {
    const SW_RENDERER = /swiftshader|llvmpipe|softpipe|software rasterizer|basic render/i;
    const SPOOF_VENDOR = IS_MAC ? 'Google Inc. (Apple)' : 'Google Inc. (Intel)';
    const SPOOF_RENDERER = IS_MAC
      ? 'ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version)'
      : IS_WIN
        ? 'ANGLE (Intel, Intel(R) UHD Graphics 630 Direct3D11 vs_5_0 ps_5_0, D3D11)'
        : 'ANGLE (Intel, Mesa Intel(R) UHD Graphics 630 (CML GT2), OpenGL 4.6)';
    const patchGetParameter = (proto) => {
      if (!proto || typeof proto.getParameter !== 'function') return;
      const realGetParameter = proto.getParameter;
      proto.getParameter = asNative(function getParameter(param) {
        const value = realGetParameter.call(this, param);
        if (
          typeof value === 'string' &&
          SW_RENDERER.test(value) &&
          (param === 37445 || param === 37446)
        ) {
          return param === 37445 ? SPOOF_VENDOR : SPOOF_RENDERER;
        }
        return value;
      }, 'getParameter');
    };
    patchGetParameter(WebGLRenderingContext.prototype);
    if (typeof WebGL2RenderingContext !== 'undefined') {
      patchGetParameter(WebGL2RenderingContext.prototype);
    }
  } catch (_e) {}

  // ── window.outerWidth / outerHeight ────────────────────────────────
  // Headless reports 0, or values smaller than the inner viewport — both
  // impossible for a real window. Patch on the owning prototype so the
  // descriptor location matches the real interface.
  try {
    if (outerWidth === 0 || outerHeight === 0 || outerWidth < innerWidth || outerHeight < innerHeight) {
      const owner = ownerOf(window, 'outerWidth');
      defineGetter(owner, 'outerWidth', () => innerWidth);
      defineGetter(owner, 'outerHeight', () => innerHeight + CHROME_UI_HEIGHT);
    }
  } catch (_e) {}

  // ── screen.availHeight ─────────────────────────────────────────────
  // Real macOS reserves the menubar strip (25 px) from the available
  // area; headless reports the full height. Only patch when the values
  // are identical, which is impossible on macOS.
  try {
    const TOP_INSET = IS_MAC ? 25 : 0;
    if (TOP_INSET > 0 && screen.availHeight === screen.height) {
      defineGetter(ownerOf(screen, 'availHeight'), 'availHeight', function () {
        return this.height - TOP_INSET;
      });
      if (screen.availTop === 0) {
        defineGetter(ownerOf(screen, 'availTop'), 'availTop', function () {
          return TOP_INSET;
        });
      }
    }
  } catch (_e) {}

  // ── navigator.deviceMemory ─────────────────────────────────────────
  // The spec caps the reported value at 8; headless leaks the physical
  // amount (this box reports 32). Clamp only out-of-range values.
  try {
    if (navigator.deviceMemory === undefined || navigator.deviceMemory > 8) {
      defineGetter(Navigator.prototype, 'deviceMemory', () => 8);
    }
  } catch (_e) {}

  // ── navigator.maxTouchPoints ───────────────────────────────────────
  // Desktop Chrome reports 0; headless reports 1 on some builds.
  try {
    if (navigator.maxTouchPoints > 0) {
      defineGetter(Navigator.prototype, 'maxTouchPoints', () => 0);
    }
  } catch (_e) {}

  // ── navigator.connection ───────────────────────────────────────────
  // Headless never runs the network-quality estimator, so it emits the
  // NQE compile-time defaults (rtt 100 / downlink ~1.5). Move them to
  // common real-wifi readings when the defaults are detected verbatim.
  try {
    const conn = navigator.connection;
    if (
      conn &&
      conn.rtt === 100 &&
      (conn.downlink === 1.5 || conn.downlink === 1.65)
    ) {
      const proto = Object.getPrototypeOf(conn);
      defineGetter(proto, 'rtt', () => 50);
      defineGetter(proto, 'downlink', () => 10);
    }
  } catch (_e) {}

  // ── speechSynthesis.getVoices ──────────────────────────────────────
  // Headless ships with an empty voice list; macOS Chrome returns ~150.
  // Voices are built on the real prototype so instanceof checks pass.
  try {
    if (
      typeof speechSynthesis !== 'undefined' &&
      typeof SpeechSynthesisVoice !== 'undefined' &&
      speechSynthesis.getVoices().length === 0
    ) {
      const mkVoice = (name, lang, isDefault) => {
        const voice = Object.create(SpeechSynthesisVoice.prototype);
        defineGetters(voice, {
          voiceURI: name,
          name,
          lang,
          localService: true,
          default: isDefault,
        });
        return voice;
      };
      const voices = [
        mkVoice('Samantha', 'en-US', true),
        mkVoice('Alex', 'en-US', false),
        mkVoice('Fred', 'en-US', false),
        mkVoice('Victoria', 'en-US', false),
        mkVoice('Daniel', 'en-GB', false),
        mkVoice('Karen', 'en-AU', false),
        mkVoice('Moira', 'en-IE', false),
        mkVoice('Tessa', 'en-ZA', false),
        mkVoice('Rishi', 'en-IN', false),
        mkVoice('Fiona', 'en-scotland', false),
      ];
      defineMethod(
        Object.getPrototypeOf(speechSynthesis),
        'getVoices',
        function getVoices() {
          return voices;
        },
      );
    }
  } catch (_e) {}

  // ── mediaDevices.enumerateDevices ──────────────────────────────────
  // Real hardware enumerates a camera and mic even without labels;
  // headless returns an empty list. Fabricate the default devices on the
  // real MediaDeviceInfo prototype.
  try {
    const devices = navigator.mediaDevices;
    if (
      devices &&
      typeof devices.enumerateDevices === 'function' &&
      typeof MediaDeviceInfo !== 'undefined'
    ) {
      const proto = ownerOf(devices, 'enumerateDevices');
      const realEnumerate = devices.enumerateDevices.bind(devices);
      const mkDevice = (deviceId, kind) => {
        const device = Object.create(MediaDeviceInfo.prototype);
        defineGetters(device, { deviceId, groupId: '', kind, label: '' });
        device.toJSON = asNative(
          () => ({ deviceId, groupId: '', kind, label: '' }),
          'toJSON',
        );
        return device;
      };
      defineMethod(proto, 'enumerateDevices', function enumerateDevices() {
        return realEnumerate().then((real) => {
          if (real.length > 0) return real;
          return [
            mkDevice('default', 'audioinput'),
            mkDevice('default', 'videoinput'),
            mkDevice('default', 'audiooutput'),
          ];
        });
      });
    }
  } catch (_e) {}
})();
