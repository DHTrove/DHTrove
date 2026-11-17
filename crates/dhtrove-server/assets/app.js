"use strict";
(function () {
  const get = (id) => document.getElementById(id);
  const fmt = new Intl.NumberFormat("en-US");
  let token = sessionStorage.getItem("dhtrove.token") || "";
  let connected = false;
  let phase = "stopped";
  let lastPackets = 0;
  let data = Array(30).fill(0);
  let toastTimeout = null;

  function toast(text) {
    get("toast").textContent = text;
    get("toast").classList.remove("hidden");
    clearTimeout(toastTimeout);
    toastTimeout = setTimeout(function () { get("toast").classList.add("hidden"); }, 4600);
  }
  function count(value) { return fmt.format(Number(value || 0)); }
  function bytes(value) {
    let num = Number(value || 0);
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let index = 0;
    while (num >= 1024 && index < units.length - 1) { num /= 1024; index++; }
    return num.toFixed(index ? 1 : 0) + " " + units[index];
  }
  function uptime(seconds) {
    if (seconds == null) return "—";
    const days = Math.floor(seconds / 86400);
    const hours = Math.floor(seconds % 86400 / 3600);
    const minutes = Math.floor(seconds % 3600 / 60);
    const secs = seconds % 60;
    const two = (n) => String(n).padStart(2, "0");
    return (days ? days + "d " : "") + two(hours) + ":" + two(minutes) + ":" + two(secs);
  }

  async function request(path, options) {
    const opts = options || {};
    const response = await fetch("/api" + path, {
      method: opts.method || "GET",
      body: opts.body,
      headers: Object.assign({
        "Authorization": "Bearer " + token,
        "Accept": "application/json",
        "X-DHTrove-Request": "1"
      }, opts.body ? { "Content-Type": "application/json" } : {}),
      cache: "no-store"
    });
    if (response.status === 401) {
      connected = false;
      get("auth").classList.remove("hidden");
      throw new Error("Invalid API token");
    }
    const result = await response.json().catch(function () { return {}; });
    if (!response.ok) throw new Error(result.error || "HTTP " + response.status);
    return result;
  }

  function graph(packets) {
    const delta = Math.max(0, packets - lastPackets);
    lastPackets = packets;
    data.push(delta);
    data = data.slice(-30);
    const maximum = Math.max(1, ...data);
    const points = data.map(function (n, index) {
      return (index * 560 / 29).toFixed(1) + "," + (111 - n / maximum * 104).toFixed(1);
    }).join(" ");
    get("graph-line").setAttribute("points", points);
  }

  function render(status) {
    connected = true;
    get("auth").classList.add("hidden");
    phase = status.phase;
    const running = phase === "running";
    const stopping = phase === "stopping";
    const label = stopping ? "Stopping…" : running ? "Running" : "Stopped";
    const className = "dot" + (running ? " active" : stopping ? " stopping" : "");
    get("side-light").className = className;
    get("status-light").className = className;
    get("side-state").textContent = label;
    get("status-title").textContent = label;
    get("version").textContent = "v" + status.version;
    get("status-message").textContent = stopping
      ? "Finishing work and synchronizing the database."
      : running ? "Discovering infohashes and indexing verified metadata."
      : "Discovery is not running. Start it whenever you need.";
    get("start").disabled = running || stopping;
    get("stop").disabled = !running;
    get("save").disabled = running || stopping;
    get("form-help").textContent = running || stopping
      ? "Stop the crawler to update network or storage limits."
      : "Settings are saved in the persistent data directory.";
    document.querySelectorAll("#settings-form input").forEach(function (input) {
      input.disabled = running || stopping;
    });
    get("uptime").textContent = uptime(status.uptime_seconds);
    get("last-sync").textContent = "Updated " + new Date().toLocaleTimeString();
    get("crawler-error").classList.toggle("hidden", !status.last_error);
    get("crawler-error").textContent = status.last_error ? "Last error: " + status.last_error : "";
    const stats = status.metrics || {};
    ["sightings", "records_inserted", "verified_nodes", "queued_hashes",
      "dht_packets", "dht_queries", "active_fetches", "metadata_fetched",
      "duplicates_skipped", "fetch_failures"].forEach(function (key) {
      get(key).textContent = count(stats[key]);
    });
    get("bytes_written").textContent = bytes(stats.bytes_written);
    graph(Number(stats.dht_packets || 0));
  }

  async function refresh() {
    if (!token) return;
    try {
      const response = await request("/status");
      render(response);
    } catch (error) {
      get("last-sync").textContent = "Connection interrupted";
      if (connected) toast(error.message);
      connected = false;
    }
  }

  async function loadSettings() {
    const settings = await request("/settings");
    document.querySelectorAll("#settings-form input[name]").forEach(function (input) {
      input.value = settings[input.name] == null ? "" : String(settings[input.name]);
    });
  }

  get("start").addEventListener("click", async function () {
    get("start").disabled = true;
    try {
      await request("/crawler/start", { method: "POST" });
      toast("Crawler start requested");
      await refresh();
    } catch (error) { toast(error.message); }
  });

  get("stop").addEventListener("click", async function () {
    get("stop").disabled = true;
    try {
      await request("/crawler/stop", { method: "POST" });
      toast("Crawler stop requested");
      await refresh();
    } catch (error) { toast(error.message); }
  });

  get("auth-form").addEventListener("submit", async function (event) {
    event.preventDefault();
    token = get("token").value.trim();
    get("auth-error").textContent = "";
    try {
      const result = await request("/status");
      sessionStorage.setItem("dhtrove.token", token);
      render(result);
      await loadSettings();
    } catch (error) {
      get("auth-error").textContent = error.message;
    }
  });

  document.querySelectorAll("[data-view]").forEach(function (button) {
    button.addEventListener("click", async function () {
      const destination = button.dataset.view;
      document.querySelectorAll("[data-view]").forEach(function (item) {
        item.classList.toggle("active", item === button);
      });
      get("overview").classList.toggle("hidden", destination !== "overview");
      get("settings").classList.toggle("hidden", destination !== "settings");
      get("breadcrumb").textContent = destination === "overview" ? "Overview" : "Settings";
      if (destination === "settings" && connected) {
        try { await loadSettings(); } catch (error) { toast(error.message); }
      }
    });
  });

  get("settings-form").addEventListener("submit", async function (event) {
    event.preventDefault();
    if (phase !== "stopped") { toast("Stop the crawler before saving settings"); return; }
    const settings = {};
    document.querySelectorAll("#settings-form input[name]").forEach(function (input) {
      settings[input.name] = input.name === "max_disk_gb" && input.value === ""
        ? null : Number(input.value);
    });
    try {
      await request("/settings", { method: "PUT", body: JSON.stringify(settings) });
      toast("Settings saved. Changes apply on the next start.");
    } catch (error) { toast(error.message); }
  });

  if (token) {
    refresh().then(function () {
      if (connected) loadSettings().catch(function () {});
    });
  } else {
    get("auth").classList.remove("hidden");
  }
  setInterval(refresh, 2000);
})();
