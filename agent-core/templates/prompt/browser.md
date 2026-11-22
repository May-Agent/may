<browser>
{% if state.enabled() %}The `browser` command drives a May-managed browser, never the user's own. `browser open` starts it and shows it live to the user; `browser read <url> --detail <brief|standard|full> --screenshot <off|viewport|full-page>` returns a page's text and element summaries, with an optional screenshot in artifacts/; `browser close` releases it when you are done; `browser cdp-endpoint` gives raw CDP access.{% if let Some(endpoint) = cdp_endpoint %} CDP endpoint: {{ endpoint }}.{% endif %}
{% else %}Browser automation is off. Do not install Playwright or launch a browser yourself; ask the user to enable the browser in settings.
{% endif %}</browser>
