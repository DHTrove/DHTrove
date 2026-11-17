"use strict";
(function () {
  const get = (id) => document.getElementById(id);
  const fmt = new Intl.NumberFormat("en-US");
  let connected = false;
  let user = null;
  let mode = "login";
  let setupRequired = false;
  let registrationEnabled = true;
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
      credentials: "same-origin",
      headers: Object.assign({
        "Accept": "application/json",
        "X-DHTrove-Request": "1"
      }, opts.body ? { "Content-Type": "application/json" } : {}),
      cache: "no-store"
    });
    if (response.status === 401 && !path.startsWith("/auth/login")) {
      connected = false;
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
    get("start").disabled = running || stopping || !user?.is_admin;
    get("stop").disabled = !running || !user?.is_admin;
    get("save").disabled = running || stopping || !user?.is_admin;
    document.querySelector(".actions").classList.toggle("hidden", !user?.is_admin);
    get("form-help").textContent = running || stopping
      ? "Stop the crawler to update network or storage limits."
      : "Settings are saved in the persistent data directory.";
    document.querySelectorAll("#settings-form input").forEach(function (input) {
      input.disabled = running || stopping || !user?.is_admin;
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
    if (!connected) return;
    try {
      const response = await request("/status");
      render(response);
    } catch (error) {
      get("last-sync").textContent = "Connection interrupted";
      if (connected) toast(error.message);
      connected = false;
      if (error.message.includes("HTTP 401")) await showAuth();
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

  function setAuthMode(next) {
    mode = setupRequired ? "register" : next;
    const register = mode === "register";
    get("auth-eyebrow").textContent = setupRequired ? "FIRST-TIME SETUP" : "ACCOUNT ACCESS";
    get("auth-title").textContent = setupRequired ? "Create administrator"
      : register ? "Create account" : "Sign in to DHTrove";
    get("auth-description").textContent = setupRequired
      ? "Set up the first account. It will automatically receive administrator permissions."
      : register ? "Create a regular account to access your DHTrove instance."
        : "Enter your username and password to continue.";
    get("auth-submit").textContent = setupRequired ? "Create administrator"
      : register ? "Create account" : "Sign in";
    get("auth-password").autocomplete = register ? "new-password" : "current-password";
    get("confirm-row").classList.toggle("hidden", !register);
    get("auth-confirm").required = register;
    const showSwitch = !setupRequired && (mode === "register" || registrationEnabled);
    get("auth-switch").classList.toggle("hidden", !showSwitch);
    get("auth-switch").textContent = register
      ? "Already have an account? Sign in"
      : "Don't have an account? Register";
    get("auth-error").textContent = "";
  }

  async function showAuth() {
    connected = false;
    user = null;
    get("auth").classList.remove("hidden");
    get("side-account").classList.add("hidden");
    try {
      const status = await request("/auth/setup");
      setupRequired = status.needs_setup;
      registrationEnabled = status.registration_enabled;
    } catch (error) {
      get("auth-error").textContent = "Could not reach DHTrove: " + error.message;
    }
    setAuthMode(setupRequired ? "register" : "login");
  }

  function signedIn(account) {
    user = account;
    connected = true;
    get("auth").classList.add("hidden");
    get("side-account").classList.remove("hidden");
    get("account-name").textContent = account.username;
    get("account-role").textContent = account.is_admin ? "Administrator" : "Member";
    get("users-nav").classList.toggle("hidden", !account.is_admin);
    document.querySelector(".actions").classList.toggle("hidden", !account.is_admin);
    if (!account.is_admin && !get("users").classList.contains("hidden")) {
      document.querySelector('[data-view="overview"]').click();
    }
    refresh();
    loadSettings().catch(function () {});
  }

  get("auth-switch").addEventListener("click", function () {
    setAuthMode(mode === "login" ? "register" : "login");
  });

  get("auth-form").addEventListener("submit", async function (event) {
    event.preventDefault();
    const username = get("auth-username").value.trim();
    const password = get("auth-password").value;
    if (mode === "register" && password !== get("auth-confirm").value) {
      get("auth-error").textContent = "Passwords do not match";
      return;
    }
    get("auth-submit").disabled = true;
    get("auth-error").textContent = "";
    try {
      const path = mode === "register" ? "/auth/register" : "/auth/login";
      const response = await request(path, {
        method: "POST",
        body: JSON.stringify({ username, password })
      });
      get("auth-password").value = "";
      get("auth-confirm").value = "";
      signedIn(response.user);
      if (mode === "register") toast(setupRequired ? "Administrator account created" : "Account created");
      setupRequired = false;
    } catch (error) {
      get("auth-error").textContent = error.message;
      if (error.message.includes("403") || error.message.includes("409")) {
        request("/auth/setup").then(function (info) {
          setupRequired = info.needs_setup;
          registrationEnabled = info.registration_enabled;
          if (setupRequired) setAuthMode("register");
        }).catch(function () {});
      }
    } finally {
      get("auth-submit").disabled = false;
    }
  });

  get("logout").addEventListener("click", async function () {
    try { await request("/auth/logout", { method: "POST" }); }
    catch (error) { toast(error.message); }
    await showAuth();
  });

  async function loadUsers() {
    if (!user?.is_admin) return;
    const [members, setup] = await Promise.all([
      request("/admin/users"),
      request("/auth/setup")
    ]);
    get("registration-enabled").checked = setup.registration_enabled;
    get("registration-label").textContent = setup.registration_enabled ? "Enabled" : "Disabled";
    const container = get("users-list");
    container.replaceChildren();
    for (const member of members.users) {
      const row = document.createElement("div");
      row.className = "users-row";
      const name = document.createElement("span");
      name.className = "username";
      name.textContent = member.username;
      const role = document.createElement("span");
      role.className = "role-tag" + (member.is_admin ? " admin" : "");
      role.textContent = member.is_admin ? "Administrator" : "Member";
      row.append(name, role);
      container.append(row);
    }
  }

  get("registration-enabled").addEventListener("change", async function () {
    if (!user?.is_admin) return;
    const enabled = get("registration-enabled").checked;
    try {
      await request("/admin/registration", {
        method: "POST",
        body: JSON.stringify({ enabled })
      });
      get("registration-label").textContent = enabled ? "Enabled" : "Disabled";
      toast(enabled ? "Registration enabled" : "Registration disabled");
    } catch (error) {
      get("registration-enabled").checked = !enabled;
      toast(error.message);
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
      get("users").classList.toggle("hidden", destination !== "users");
      get("breadcrumb").textContent = destination === "overview" ? "Overview"
        : destination === "settings" ? "Settings" : "Users";
      if (destination === "settings" && connected) {
        try { await loadSettings(); } catch (error) { toast(error.message); }
      }
      if (destination === "users" && connected) {
        try { await loadUsers(); } catch (error) { toast(error.message); }
      }
    });
  });

  get("settings-form").addEventListener("submit", async function (event) {
    event.preventDefault();
    if (phase !== "stopped" || !user?.is_admin) { toast("Administrator access and a stopped crawler are required"); return; }
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

  async function initialize() {
    try {
      const result = await request("/auth/me");
      signedIn(result.user);
    } catch {
      await showAuth();
    }
  }
  initialize();
  setInterval(refresh, 2000);
})();
