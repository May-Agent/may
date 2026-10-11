// Browser-fingerprint hardening shim, injected via
// Page.addScriptToEvaluateOnNewDocument before any page script runs.
//
// Targets the half-dozen fingerprints that puppeteer-extra-plugin-stealth
// (MIT, https://github.com/berstend/puppeteer-extra) covers in its core
// "page-evasions" set. Vendored here rather than pulled at runtime so
// the patch surface is auditable and version-stable.

(() => {
  // navigator.webdriver — primary automation tell. Headless Chrome sets
  // it to true; we pretend the user is driving with a normal browser.
  try {
    Object.defineProperty(Navigator.prototype, 'webdriver', {
      get: () => undefined,
      configurable: true,
    });
  } catch (_e) {}

  // chrome.runtime — present in real Chrome; absent in headless. Stub
  // just enough that "is chrome.runtime defined" probes pass.
  try {
    if (typeof window.chrome === 'undefined') {
      Object.defineProperty(window, 'chrome', {
        value: {},
        writable: true,
        configurable: true,
        enumerable: true,
      });
    }
    if (typeof window.chrome.runtime === 'undefined') {
      window.chrome.runtime = {
        OnInstalledReason: { CHROME_UPDATE: 'chrome_update' },
        OnRestartRequiredReason: { APP_UPDATE: 'app_update' },
        PlatformArch: { ARM: 'arm', ARM64: 'arm64', X86_32: 'x86-32', X86_64: 'x86-64' },
        connect: () => undefined,
        sendMessage: () => undefined,
      };
    }
  } catch (_e) {}

  // navigator.plugins / mimeTypes — empty arrays scream automation.
  try {
    const plugins = [
      { name: 'PDF Viewer', filename: 'internal-pdf-viewer', description: 'Portable Document Format' },
      { name: 'Chrome PDF Viewer', filename: 'internal-pdf-viewer', description: '' },
      { name: 'Chromium PDF Viewer', filename: 'internal-pdf-viewer', description: '' },
    ];
    Object.defineProperty(Navigator.prototype, 'plugins', {
      get: () => plugins,
      configurable: true,
    });
    Object.defineProperty(Navigator.prototype, 'mimeTypes', {
      get: () => [
        { type: 'application/pdf', suffixes: 'pdf', description: '' },
      ],
      configurable: true,
    });
  } catch (_e) {}

  // navigator.languages — empty in some headless builds.
  try {
    Object.defineProperty(Navigator.prototype, 'languages', {
      get: () => ['en-US', 'en'],
      configurable: true,
    });
  } catch (_e) {}

  // Permissions.query for "notifications" returns "denied" in headless;
  // real browsers return "default" until the user picks. Match that.
  try {
    const originalQuery = window.navigator.permissions
      ? window.navigator.permissions.query.bind(window.navigator.permissions)
      : null;
    if (originalQuery) {
      window.navigator.permissions.query = (parameters) =>
        parameters && parameters.name === 'notifications'
          ? Promise.resolve({ state: 'default' })
          : originalQuery(parameters);
    }
  } catch (_e) {}

  // WebGL UNMASKED_VENDOR_WEBGL / UNMASKED_RENDERER_WEBGL — return realistic
  // strings rather than the SwiftShader / Google strings headless reports.
  try {
    const patchGetParameter = (proto) => {
      if (!proto || !proto.getParameter) return;
      const original = proto.getParameter;
      proto.getParameter = function (param) {
        if (param === 37445) return 'Intel Inc.';
        if (param === 37446) return 'Intel Iris OpenGL Engine';
        return original.call(this, param);
      };
    };
    patchGetParameter(WebGLRenderingContext.prototype);
    if (typeof WebGL2RenderingContext !== 'undefined') {
      patchGetParameter(WebGL2RenderingContext.prototype);
    }
  } catch (_e) {}
})();
