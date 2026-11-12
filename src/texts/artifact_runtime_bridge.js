(() => {
  const SOURCE = 'may-artifact-runtime';
  const MAX_DIAGNOSTICS = 16;
  const MAX_DIAGNOSTIC_LENGTH = 2000;
  const DIAGNOSTIC_TRUNCATION_SUFFIX = ' ... [truncated]';
  const SLIDEV_PRELOAD_ATTEMPTS = 40;
  const GENERIC_READY_ATTEMPTS = 5;
  const MAX_ATTEMPTS = 50;
  const script = document.currentScript;
  const artifactType = script?.dataset.artifactType;
  const validationProfile = script?.dataset.validationProfile;
  if (!artifactType) throw new Error('artifact runtime bridge requires data-artifact-type');
  if (validationProfile !== 'generic' && validationProfile !== 'slidev') {
    throw new Error(`unsupported artifact validation profile: ${validationProfile}`);
  }

  const diagnostics = new Set();
  let ready = false;
  let publishTimer;
  let lastPublished = '';
  const normalize = (value) => typeof value === 'string' ? value.replace(/\s+/g, ' ').trim() : '';
  const describe = (value) => {
    if (value instanceof Error) return normalize(value.stack || value.message);
    if (typeof value === 'string') return normalize(value);
    try {
      return normalize(JSON.stringify(value));
    } catch {
      return normalize(String(value));
    }
  };
  const truncateDiagnostic = (value) => value.length <= MAX_DIAGNOSTIC_LENGTH
    ? value
    : `${value.slice(0, MAX_DIAGNOSTIC_LENGTH - DIAGNOSTIC_TRUNCATION_SUFFIX.length)}${DIAGNOSTIC_TRUNCATION_SUFFIX}`;
  const addDiagnostic = (prefix, values) => {
    const detail = normalize(values.map(describe).filter(Boolean).join(' '));
    if (!detail) return;
    diagnostics.add(truncateDiagnostic(`${prefix}: ${detail}`));
    if (ready) schedulePublish();
  };
  const currentDiagnostics = () => [...diagnostics].slice(0, MAX_DIAGNOSTICS);
  const publish = () => {
    publishTimer = undefined;
    const next = currentDiagnostics();
    const fingerprint = JSON.stringify(next);
    if (fingerprint === lastPublished) return;
    lastPublished = fingerprint;
    window.parent.postMessage({ source: SOURCE, version: 1, artifactType, diagnostics: next }, '*');
  };
  const schedulePublish = () => {
    if (publishTimer !== undefined) clearTimeout(publishTimer);
    publishTimer = setTimeout(publish, 250);
  };

  const originalConsoleError = console.error.bind(console);
  console.error = (...values) => {
    addDiagnostic('console.error', values);
    originalConsoleError(...values);
  };
  window.addEventListener('error', (event) => {
    const location = event.filename ? `${event.filename}:${event.lineno || 0}:${event.colno || 0}` : '';
    addDiagnostic('Uncaught error', [event.error || event.message, location]);
  });
  window.addEventListener('unhandledrejection', (event) => {
    addDiagnostic('Unhandled promise rejection', [event.reason]);
  });

  const inspectSlidev = () => {
    const slides = Array.from(document.querySelectorAll('.slidev-page'));
    if (slides.length === 0) return false;
    const bodyText = normalize(document.body?.innerText);
    const runtimeError = bodyText.match(/(?:Syntax error in text|Parse error on line[^.]*|MermaidDetailedError[^.]*)/i)?.[0];
    if (runtimeError) addDiagnostic('Slidev runtime error', [runtimeError]);
    const explicitErrors = Array.from(document.querySelectorAll('.mermaid-error, [data-mermaid-error], .slidev-error'))
      .map((node) => normalize(node.textContent))
      .filter(Boolean);
    explicitErrors.forEach((message) => addDiagnostic('Slidev runtime error', [message]));
    slides.forEach((slide, index) => {
      const visibleText = normalize(slide.innerText);
      const visibleMedia = slide.querySelector('img, svg, video, canvas, iframe');
      if (!visibleText && !visibleMedia) addDiagnostic('Artifact validation error', [`Rendered Slidev slide ${index + 1} is blank.`]);
    });
    return true;
  };

  let attempts = 0;
  const poll = () => {
    attempts += 1;
    const requiredAttempts = validationProfile === 'slidev' ? SLIDEV_PRELOAD_ATTEMPTS : GENERIC_READY_ATTEMPTS;
    if (attempts >= requiredAttempts) {
      const rendered = validationProfile === 'generic' || inspectSlidev();
      if (rendered) {
        ready = true;
        publish();
        return;
      }
    }
    if (attempts >= MAX_ATTEMPTS) {
      addDiagnostic('Artifact validation error', ['Artifact runtime did not render within 5 seconds.']);
      ready = true;
      publish();
      return;
    }
    setTimeout(poll, 100);
  };
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', poll, { once: true });
  else poll();
})();
