<script lang="ts">
  // One way in, three shapes: a password, a key file, the running agent.
  //
  // Shared by Add machines and Update over SSH so the two never disagree
  // about what a credential looks like. Nothing here is remembered: the
  // draft lives in the sheet that owns it and dies with it.
  import { api, errText, type DeployAuthDraft } from "../api";
  import Icon from "./Icon.svelte";
  import Hint from "./Hint.svelte";

  let {
    auth = $bindable(),
    complete = $bindable(false),
  }: {
    auth: DeployAuthDraft;
    /** Out: the login method has what it needs — typed, picked, or waiting
     *  in the keychain. The sheet's button reads this, not the draft: a
     *  draft filled from the keychain carries no password by design (S3),
     *  and a button that read only the draft stayed dead until the person
     *  retyped what was already remembered. */
    complete?: boolean;
  } = $props();

  let showPw = $state(false);
  let showSudo = $state(false);

  // -- the keychain --------------------------------------------------------------
  //
  // Names with a remembered record are offered as the user is typed; the
  // record itself is fetched only when the name matches one, so nothing
  // is read from the keychain for a user nobody remembered. Filling in
  // says so, and "forget" is one click beside it.
  //
  // What comes back is *which* secrets exist, never the secrets themselves
  // (S3): a remembered password stays in the OS store until the deploy
  // itself asks for it, on the Rust side. So the fields below stay empty
  // and say "remembered" instead of showing a row of dots that was, until
  // now, a real password sitting in this window.
  let remembered = $state<string[]>([]);
  let filledFrom = $state<string | null>(null);
  let keychainErr = $state<string | null>(null);
  /** Which fields the keychain will fill in, while `auth.fromKeychain`. */
  let has = $state({ password: false, passphrase: false, sudoPassword: false });
  /** The key file to show. `auth.keyPath` carries only a picker token
   *  (SEC-14) — the person's pick, or the one the backend made for the
   *  record's file. */
  let keyShown = $state<string | null>(null);
  $effect(() => {
    void api.credentialUsers().then((u) => (remembered = u)).catch(() => (remembered = []));
  });
  let fillTimer: ReturnType<typeof setTimeout> | null = null;
  $effect(() => {
    const user = auth.user.trim();
    if (fillTimer) clearTimeout(fillTimer);
    if (!user || filledFrom === user || !remembered.includes(user)) return;
    fillTimer = setTimeout(() => void fill(user), 200);
  });
  async function fill(user: string) {
    try {
      const saved = await api.credentialGet(user);
      if (!saved || auth.user.trim() !== user) return;
      has = {
        password: saved.hasPassword,
        passphrase: saved.hasPassphrase,
        sudoPassword: saved.hasSudoPassword,
      };
      auth = {
        ...auth,
        method: saved.method,
        // A token for the record's key file, so the key is the draft's own
        // and stays put whatever happens to the secrets beside it.
        keyPath: saved.keyToken,
        password: null,
        passphrase: null,
        sudoPassword: null,
        remember: true,
        fromKeychain: deferring(),
      };
      keyShown = saved.keyPath;
      filledFrom = user;
      keychainErr = null;
    } catch (e) {
      keychainErr = errText(e);
    }
  }
  /** Is anything still left to the keychain? */
  function deferring(): boolean {
    return has.password || has.passphrase || has.sudoPassword;
  }
  /** Type this one field instead of the remembered value — the rest, and
   *  the key file, stay as they were.
   *
   *  This used to stop deferring to the keychain for every field at once
   *  and drop the record's key file with it, so a changed passphrase meant
   *  picking the key again and retyping a sudo password that had not
   *  changed. A typed field beats a remembered one on the Rust side
   *  (`resolve_secrets`), so the others can go on deferring. */
  function typeInstead(field: keyof typeof has) {
    has = { ...has, [field]: false };
    // Nothing left to fetch: stop asking, so a machine with no record of
    // its own does not refuse a run that no longer needs one.
    auth = { ...auth, fromKeychain: deferring() };
    // `filledFrom` stays: it is what tells the effect above this user has
    // been looked up. Clearing it re-armed the fill, which put the record
    // back 200ms later and closed the field the person had just opened.
  }
  async function forget() {
    const user = auth.user.trim();
    try {
      await api.credentialForget(user);
      remembered = remembered.filter((u) => u !== user);
      filledFrom = null;
      // The key file stays for this run: it is on screen, it is the
      // draft's own token, and forgetting is about the keychain.
      has = { password: false, passphrase: false, sudoPassword: false };
      auth = {
        ...auth,
        password: null,
        passphrase: null,
        sudoPassword: null,
        remember: false,
        fromKeychain: false,
      };
    } catch (e) {
      keychainErr = errText(e);
    }
  }
  const METHODS: [string, string][] = [
    ["password", "Password"],
    ["key", "Key file"],
    ["agent", "ssh-agent"],
  ];

  // Key and agent logins have no password to hand sudo, so the sudo
  // field is open by default there; with a password login it is one
  // click away for the machine where the two differ.
  const sudoWanted = $derived(auth.method !== "password" || showSudo);

  $effect(() => {
    complete =
      auth.method === "password"
        ? !!auth.password || (auth.fromKeychain && has.password)
        : auth.method === "key"
          ? !!auth.keyPath
          : true;
  });

  async function pickKey() {
    const f = await api.pickFile("Pick your private key");
    if (f) {
      auth.keyPath = f.token;
      keyShown = f.path;
    }
  }
</script>

<div class="space-y-3">
  <div class="grid gap-3 sm:grid-cols-[1fr_auto]">
    <label class="block">
      <span class="mb-1.5 block text-sm text-ink-4">User</span>
      <input
        bind:value={auth.user}
        class="field field-sunken h-10 w-full px-3 text-base outline-none"
        placeholder="root"
        autocapitalize="off"
        autocorrect="off"
        spellcheck="false"
        list="mw-remembered-users"
      />
      <datalist id="mw-remembered-users">
        {#each remembered as u}<option value={u}></option>{/each}
      </datalist>
      {#if filledFrom === auth.user.trim()}
        <span class="mt-1.5 flex items-center gap-2 text-xs" style="color: var(--accent-ink);">
          filled in from your keychain
          <button type="button" class="linkish text-xs" onclick={forget}>forget it</button>
        </span>
      {/if}
    </label>
    <div class="block">
      <span class="mb-1.5 block text-sm text-ink-4">Log in with</span>
      <div class="seg h-10 items-center" role="tablist" aria-label="Login method">
        {#each METHODS as [id, label] (id)}
          <button
            type="button"
            role="tab"
            class="h-8 px-3.5 text-sm"
            aria-selected={auth.method === id}
            onclick={() => (auth.method = id)}>{label}</button
          >
        {/each}
      </div>
    </div>
  </div>

  {#if auth.method === "password"}
    <label class="block">
      <span class="mb-1.5 block text-sm text-ink-4">Password</span>
      {#if auth.fromKeychain && has.password}
        <!-- Nothing to show, because nothing came here. The password stays
             in the OS store until the deploy asks the backend for it (S3). -->
        <div
          class="flex h-10 items-center justify-between gap-3 rounded-md px-3 text-sm text-ink-4"
          style="border: 1px dashed var(--edge-dashed);"
        >
          <span class="flex items-center gap-2">
            <Icon name="secrets" size={14} />
            kept in your keychain
          </span>
          <button type="button" class="linkish text-xs" onclick={() => typeInstead("password")}>
            type a different one
          </button>
        </div>
      {:else}
        <div class="relative">
          <input
            bind:value={auth.password}
            type={showPw ? "text" : "password"}
            class="field field-sunken h-10 w-full px-3 pr-10 text-base outline-none"
            autocomplete="off"
          />
          <button
            type="button"
            class="iconbtn absolute top-1/2 right-1.5 h-7 w-7 -translate-y-1/2"
            title={showPw ? "Hide the password" : "Show the password"}
            onclick={() => (showPw = !showPw)}
          >
            <Icon name="eye" size={14} />
          </button>
        </div>
      {/if}
      <span class="mt-1.5 block text-xs text-ink-5">
        Used for SSH, and for sudo unless you say otherwise. Kept in memory for this run only.
      </span>
    </label>
    {#if !showSudo}
      <button type="button" class="linkish text-xs" onclick={() => (showSudo = true)}>
        sudo uses a different password
      </button>
    {/if}
  {:else if auth.method === "key"}
    <label class="block">
      <span class="mb-1.5 block text-sm text-ink-4">Private key</span>
      <div class="flex gap-2">
        <input
          value={keyShown ?? ""}
          readonly
          title={keyShown ?? ""}
          class="field field-sunken mono h-10 min-w-0 flex-1 px-3 text-sm outline-none placeholder:font-sans"
          placeholder="pick ~/.ssh/id_ed25519 or another key"
          spellcheck="false"
        />
        <button type="button" class="btn btn-secondary h-10 shrink-0 px-4 text-sm" onclick={pickKey}>Browse…</button>
      </div>
    </label>
    <label class="block">
      <span class="mb-1.5 block text-sm text-ink-4">Passphrase <span class="text-ink-muted">(if the key has one)</span></span>
      {#if auth.fromKeychain && has.passphrase}
        <div
          class="flex h-10 items-center justify-between gap-3 rounded-md px-3 text-sm text-ink-4"
          style="border: 1px dashed var(--edge-dashed);"
        >
          <span class="flex items-center gap-2"><Icon name="secrets" size={14} /> kept in your keychain</span>
          <button type="button" class="linkish text-xs" onclick={() => typeInstead("passphrase")}>type a different one</button>
        </div>
      {:else}
        <input bind:value={auth.passphrase} type="password" class="field field-sunken h-10 w-full px-3 text-base outline-none" autocomplete="off" />
      {/if}
    </label>
  {:else}
    <div class="rounded-lg px-3.5 py-3 text-sm leading-normal text-ink-4" style="border: 1px dashed var(--edge-dashed);">
      Whatever keys your running <span class="mono">ssh-agent</span> holds are offered in turn.
      <Hint text="Director talks to the agent named by SSH_AUTH_SOCK, the same one your terminal uses. Nothing is copied out of it — the agent signs, Director sends." />
    </div>
  {/if}

  <label class="flex cursor-pointer items-center justify-between gap-4 rounded-md px-3.5 py-2.5" style="background: var(--surface-subtle); box-shadow: 0 0 0 1px var(--edge-card);">
    <span>
      <span class="block text-sm font-medium text-ink">Remember in my keychain</span>
      <span class="mt-0.5 block text-xs leading-normal text-ink-5">
        Stored by your OS keychain for this user on each of these machines, and offered next time — only for those machines. Director never writes it to disk itself.
      </span>
    </span>
    <input type="checkbox" class="switch" bind:checked={auth.remember} />
  </label>
  {#if keychainErr}
    <div class="text-xs" style="color: var(--status-warn-ink);">{keychainErr}</div>
  {/if}

  {#if sudoWanted}
    <label class="block">
      <span class="mb-1.5 flex items-center gap-1 text-sm text-ink-4">
        sudo password
        <span class="text-ink-muted">(for root, when the account is not root)</span>
        <Hint text="Root, or sudo, gets the machine the server shape: /usr/local/bin, a meshnode service account, a system unit, the firewall rule. Without it the node still installs — inside that account's home, as that account — and the card says so." />
      </span>
      {#if auth.fromKeychain && has.sudoPassword}
        <div
          class="flex h-10 items-center justify-between gap-3 rounded-md px-3 text-sm text-ink-4"
          style="border: 1px dashed var(--edge-dashed);"
        >
          <span class="flex items-center gap-2"><Icon name="secrets" size={14} /> kept in your keychain</span>
          <button type="button" class="linkish text-xs" onclick={() => typeInstead("sudoPassword")}>type a different one</button>
        </div>
      {:else}
        <input bind:value={auth.sudoPassword} type="password" class="field field-sunken h-10 w-full px-3 text-base outline-none" autocomplete="off" />
      {/if}
    </label>
  {/if}
</div>
