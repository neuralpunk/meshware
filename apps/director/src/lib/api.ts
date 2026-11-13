// Typed wrappers over the Director's Tauri commands. All row/event types
// are generated from the Rust view models (src/lib/bindings) — UI and
// engine never drift (MASTERPLAN §14.1). Every CLI verb has a counterpart
// here (API parity, MASTERPLAN §3).

import { invoke as tauriInvoke, Channel } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import type { ScriptSpecView } from "./bindings/ScriptSpecView";
import type { RetentionView } from "./bindings/RetentionView";
import type { RetentionRowView } from "./bindings/RetentionRowView";
import type { SinkView } from "./bindings/SinkView";
import type { SinkKindView } from "./bindings/SinkKindView";
import type { SinkDraft } from "./bindings/SinkDraft";
import type { HistoryJobPageView } from "./bindings/HistoryJobPageView";
import type { PayloadWindowView } from "./bindings/PayloadWindowView";
import type { HistoryJobView } from "./bindings/HistoryJobView";
import type { TierView } from "./bindings/TierView";
import type { BackpressureView } from "./bindings/BackpressureView";
import type { BootstrapView } from "./bindings/BootstrapView";
import type { IdentityLockView } from "./bindings/IdentityLockView";
import type { SecurityKeyStatusView } from "./bindings/SecurityKeyStatusView";
import type { AgentKeyView } from "./bindings/AgentKeyView";
import type { SecurityKeyPlanView } from "./bindings/SecurityKeyPlanView";
import type { SecurityKeyBoundView } from "./bindings/SecurityKeyBoundView";
import type { SecurityKeyTestView } from "./bindings/SecurityKeyTestView";
import type { SetAsideView } from "./bindings/SetAsideView";
import type { AttestRootView } from "./bindings/AttestRootView";
import type { UserHardwareView } from "./bindings/UserHardwareView";
import type { FarmBuildLine } from "./bindings/FarmBuildLine";
import type { PairCodeView } from "./bindings/PairCodeView";
import type { StatusView } from "./bindings/StatusView";
import type { WanView } from "./bindings/WanView";
import type { NodeView } from "./bindings/NodeView";
import type { ContentStatusView } from "./bindings/ContentStatusView";
import type { ContentPolicyOption } from "./bindings/ContentPolicyOption";
import type { ContentPruneView } from "./bindings/ContentPruneView";
import type { DiskView } from "./bindings/DiskView";
import type { ServiceAccountCreated } from "./bindings/ServiceAccountCreated";
import type { ServiceAccountView } from "./bindings/ServiceAccountView";
import type { OidcProviderView } from "./bindings/OidcProviderView";
import type { TopView } from "./bindings/TopView";
import type { TotalsView } from "./bindings/TotalsView";
import type { DeepLinkView } from "./bindings/DeepLinkView";
import type { JobView } from "./bindings/JobView";
import type { JobsPageView } from "./bindings/JobsPageView";
import type { StateVersionsView } from "./bindings/StateVersionsView";
import type { JobCountsView } from "./bindings/JobCountsView";
import type { ServiceView } from "./bindings/ServiceView";
import type { DeploymentView } from "./bindings/DeploymentView";
import type { DeploymentStatusView } from "./bindings/DeploymentStatusView";
import type { DeploymentCheckView } from "./bindings/DeploymentCheckView";
import type { PortLedgerViewT } from "./bindings/PortLedgerViewT";
import type { RuntimesViewT } from "./bindings/RuntimesViewT";
import type { PlacementNodeView } from "./bindings/PlacementNodeView";
import type { VolumeRowView } from "./bindings/VolumeRowView";
import type { ShedView } from "./bindings/ShedView";
import type { RegistryRowView } from "./bindings/RegistryRowView";
import type { RegistryInstanceView } from "./bindings/RegistryInstanceView";
import type { DnsStatusViewT } from "./bindings/DnsStatusViewT";
import type { ServiceScheduleView } from "./bindings/ServiceScheduleView";
import type { ServiceScheduleCheckView } from "./bindings/ServiceScheduleCheckView";
import type { GangRowView } from "./bindings/GangRowView";
import type { DeviceView } from "./bindings/DeviceView";
import type { JobExplainView } from "./bindings/JobExplainView";
import type { PlanView } from "./bindings/PlanView";
import type { CacheStatsView } from "./bindings/CacheStatsView";
import type { CacheServiceView } from "./bindings/CacheServiceView";
import type { PlacementView } from "./bindings/PlacementView";
import type { CandidateScoreView } from "./bindings/CandidateScoreView";
import type { ScoreTermView } from "./bindings/ScoreTermView";
import type { CacheHitView } from "./bindings/CacheHitView";
import type { KeyFieldView } from "./bindings/KeyFieldView";
import type { PlanDelayView } from "./bindings/PlanDelayView";
import type { BundleView } from "./bindings/BundleView";
import type { BundleFileView } from "./bindings/BundleFileView";
import type { PushResultView } from "./bindings/PushResultView";
import type { SourceView } from "./bindings/SourceView";
import type { SyncResultView } from "./bindings/SyncResultView";
import type { SecretView } from "./bindings/SecretView";
import type { SecretSetView } from "./bindings/SecretSetView";
import type { ProviderView } from "./bindings/ProviderView";
import type { UserView } from "./bindings/UserView";
import type { GrantView } from "./bindings/GrantView";
import type { AuditView } from "./bindings/AuditView";
import type { WhoAmIView } from "./bindings/WhoAmIView";
import type { EnrolmentView } from "./bindings/EnrolmentView";
import type { DefinitionView } from "./bindings/DefinitionView";
import type { RunDefaultsSavedView } from "./bindings/RunDefaultsSavedView";
import type { PrefetchEvent } from "./bindings/PrefetchEvent";
import type { UpgradeEvent } from "./bindings/UpgradeEvent";
import type { ReleaseTrustView } from "./bindings/ReleaseTrustView";
import type { ReleaseKeyView } from "./bindings/ReleaseKeyView";
import type { BuildCandidateView } from "./bindings/BuildCandidateView";
import type { PingView } from "./bindings/PingView";
import type { QueueRowView } from "./bindings/QueueRowView";
import type { DoctorCheckView } from "./bindings/DoctorCheckView";
import type { DoctorFixReport } from "./bindings/DoctorFixReport";
import type { DoctorFixView } from "./bindings/DoctorFixView";
import type { TrayStatusView } from "./bindings/TrayStatusView";
import type { AboutView } from "./bindings/AboutView";
import type { BackupView } from "./bindings/BackupView";
import type { RestoreView } from "./bindings/RestoreView";
import type { CandidateView } from "./bindings/CandidateView";
import type { InviteView } from "./bindings/InviteView";
import type { PickedView } from "./bindings/PickedView";
import type { LanWatchView } from "./bindings/LanWatchView";
import type { RunEvent } from "./bindings/RunEvent";
import type { ParallelEvent } from "./bindings/ParallelEvent";
import type { NodeProgressView } from "./bindings/NodeProgressView";
import type { CapacityView } from "./bindings/CapacityView";
import type { DrainEvent } from "./bindings/DrainEvent";
import type { WizardPairEvent } from "./bindings/WizardPairEvent";
import type { DemoEvent } from "./bindings/DemoEvent";
import type { ApproveEvent } from "./bindings/ApproveEvent";
import type { DeployDraft } from "./bindings/DeployDraft";
import type { DeployAuthDraft } from "./bindings/DeployAuthDraft";
import type { RememberedCredentialView } from "./bindings/RememberedCredentialView";
import type { DeployEvent } from "./bindings/DeployEvent";
import type { DeployPreviewView } from "./bindings/DeployPreviewView";
import type { BinarySourceView } from "./bindings/BinarySourceView";
import type { SshUpgradeDraft } from "./bindings/SshUpgradeDraft";
import type { ReleaseCheckView } from "./bindings/ReleaseCheckView";
import type { ReleaseFetchEvent } from "./bindings/ReleaseFetchEvent";
import type { DirectorUpdateView } from "./bindings/DirectorUpdateView";
import type { DirectorUpdateEvent } from "./bindings/DirectorUpdateEvent";
import type { JobPreviewView } from "./bindings/JobPreviewView";
import type { NodeFactsView } from "./bindings/NodeFactsView";
import type { FactDocView } from "./bindings/FactDocView";
import type { FactCollectorView } from "./bindings/FactCollectorView";
import type { FactChangeView } from "./bindings/FactChangeView";
import type { FactKeyChangeView } from "./bindings/FactKeyChangeView";
import type { ResultSetView } from "./bindings/ResultSetView";
import type { SessionView } from "./bindings/SessionView";
import type { DeploymentInstanceDetailView } from "./bindings/DeploymentInstanceDetailView";
import type { DeploymentHistoryView } from "./bindings/DeploymentHistoryView";
import type { DeploymentVersionView } from "./bindings/DeploymentVersionView";
import type { DeploymentTagView } from "./bindings/DeploymentTagView";
import type { InstanceStatView } from "./bindings/InstanceStatView";
import type { IncarnationView } from "./bindings/IncarnationView";
import type { SessionPolicyView } from "./bindings/SessionPolicyView";
import type { TerminalEvent } from "./bindings/TerminalEvent";
import type { ResultRowView } from "./bindings/ResultRowView";


// -- a security key that stopped signing (2026-09-18) -------------------------
//
// Every error from a signer Director paused starts with this (`KEY_PAUSED`
// in engine.rs). One place sees every command's failure, so the window
// learns it here — whichever of a dozen refreshes hit it first — and shows
// the one screen that helps, instead of a dozen error toasts.

/** What an error from a paused signer starts with. */
export const KEY_PAUSED = "security-key-paused: ";

const keyPausedListeners = new Set<(why: string) => void>();

/** Be told when Director has stopped asking the security key to sign. */
export function onKeyPaused(listener: (why: string) => void): () => void {
  keyPausedListeners.add(listener);
  return () => keyPausedListeners.delete(listener);
}

function invoke<T>(
  cmd: string,
  args?: Parameters<typeof tauriInvoke>[1],
  options?: Parameters<typeof tauriInvoke>[2],
): Promise<T> {
  return tauriInvoke<T>(cmd, args, options).catch((e: unknown) => {
    const text = typeof e === "string" ? e : e instanceof Error ? e.message : String(e);
    if (text.startsWith(KEY_PAUSED)) {
      for (const listener of keyPausedListeners) listener(text.slice(KEY_PAUSED.length));
    }
    throw e;
  });
}

export type {
  RetentionView,
  RetentionRowView,
  SinkView,
  SinkKindView,
  SinkDraft,
  HistoryJobView,
  HistoryJobPageView,
  PayloadWindowView,
  TierView,
  BackpressureView,
  ContentStatusView,
  ContentPruneView,
  ContentPolicyOption,
  DiskView,
  DirectorUpdateView,
  DirectorUpdateEvent,
  DeployDraft,
  DeployAuthDraft,
  RememberedCredentialView,
  DeployEvent,
  DeployPreviewView,
  BinarySourceView,
  SshUpgradeDraft,
  ReleaseCheckView,
  ReleaseFetchEvent,
  JobExplainView,
  PlacementView,
  CandidateScoreView,
  ScoreTermView,
  CacheHitView,
  KeyFieldView,
  CacheStatsView,
  CacheServiceView,
  PlanView,
  PlanDelayView,
  DeploymentView,
  DeploymentStatusView,
  DeploymentCheckView,
  PortLedgerViewT,
  RuntimesViewT,
  PlacementNodeView,
  VolumeRowView,
  ShedView,
  RegistryRowView,
  RegistryInstanceView,
  DnsStatusViewT,
  ServiceScheduleView,
  ServiceScheduleCheckView,
  QueueRowView,
  BootstrapView,
  IdentityLockView,
  SecurityKeyStatusView,
  AgentKeyView,
  SecurityKeyPlanView,
  SecurityKeyBoundView,
  SecurityKeyTestView,
  SetAsideView,
  AttestRootView,
  UserHardwareView,
  StatusView,
  WanView,
  NodeView,
  TopView,
  TotalsView,
  DeepLinkView,
  JobView,
  JobsPageView,
  StateVersionsView,
  JobCountsView,
  ServiceView,
  BundleView,
  BundleFileView,
  PushResultView,
  SourceView,
  SyncResultView,
  SecretView,
  SecretSetView,
  ProviderView,
  UserView,
  GrantView,
  AuditView,
  WhoAmIView,
  EnrolmentView,
  DefinitionView,
  RunDefaultsSavedView,
  PrefetchEvent,
  UpgradeEvent,
  ReleaseTrustView,
  ReleaseKeyView,
  BuildCandidateView,
  InviteView,
  PickedView,
  JobPreviewView,
  PingView,
  DoctorCheckView,
  DoctorFixReport,
  DoctorFixView,
  TrayStatusView,
  AboutView,
  BackupView,
  RestoreView,
  CandidateView,
  LanWatchView,
  RunEvent,
  ParallelEvent,
  NodeProgressView,
  CapacityView,
  DrainEvent,
  WizardPairEvent,
  DemoEvent,
  ApproveEvent,
  NodeFactsView,
  FactDocView,
  FactCollectorView,
  FactChangeView,
  FactKeyChangeView,
  ResultSetView,
  SessionView,
  DeploymentInstanceDetailView,
  DeploymentHistoryView,
  DeploymentVersionView,
  DeploymentTagView,
  InstanceStatView,
  IncarnationView,
  SessionPolicyView,
  TerminalEvent,
  ResultRowView,
};

export type Priority = "low" | "normal" | "high";

/** Shared dispatch options for `run` and `service run` (MASTERPLAN §9.3, §10.2). */
export type DispatchArgs = {
  on: string | null;
  /** null = one scheduler-picked target; 0 = every matching node; n = best n. */
  fanout: number | null;
  timeoutSecs: number | null;
  priority: Priority;
  maxParallel: number | null;
  /** "3" or "10%" */
  haltOnFail: string | null;
  /**
   * Resolve everything — targets, env keys, secret names, blast radius —
   * without dispatching. Answered with a single `preview` event.
   */
  dryRun: boolean;
};

/** Dispatch options for a real run rather than a preview. */
export const noOverride = { dryRun: false } as const;

function channelOf<T>(onEvent: (ev: T) => void): Channel<T> {
  const channel = new Channel<T>();
  channel.onmessage = onEvent;
  return channel;
}

export const api = {
  // -- the key at rest (B9) -------------------------------------------------
  //
  // The same file and the same session unlock the CLI uses, so
  // `meshnode identity unlock` in a terminal and the dialog in Director are
  // two doors onto one state rather than two mechanisms.
  identityLockStatus: () => invoke<IdentityLockView>("identity_lock_status"),
  identityUnlock: (passphrase: string) =>
    invoke<IdentityLockView>("identity_unlock", { passphrase }),
  identityLockNow: () => invoke<IdentityLockView>("identity_lock_now"),
  identityProtect: (passphrase: string) =>
    invoke<IdentityLockView>("identity_protect", { passphrase }),
  identityUnprotect: (passphrase: string) =>
    invoke<IdentityLockView>("identity_unprotect", { passphrase }),

  // -- the security key (Y2/Y3) ---------------------------------------------
  //
  // `meshnode identity hardware` and `users attest-root`, for the window.
  // The rules are the CLI's: `mesh_core::hardware` decides what may be
  // bound on both sides.

  /** Where this account's key is, and what the agent is holding. */
  securityKeyStatus: () => invoke<SecurityKeyStatusView>("security_key_status"),
  /** What binding one of the agent's keys would take. Changes nothing. */
  securityKeyPlan: (key: string) => invoke<SecurityKeyPlanView>("security_key_plan", { key }),
  /** Bind to it — letting it into the mesh first where the plan says so. */
  securityKeyUse: (key: string) => invoke<SecurityKeyBoundView>("security_key_use", { key }),
  /** Publish the proof: `certs` are `pickFile` tokens, leaf first. */
  securityKeyAttest: (certs: string[], kind: string, slot: string) =>
    invoke<void>("security_key_attest", { certs, kind, slot }),
  /** Ask a key to sign twice before anything changes: can it, and will it
   *  ask for a PIN or touch every time? */
  securityKeyTest: (key: string) => invoke<SecurityKeyTestView>("security_key_test", { key }),
  /** Connect a YubiKey to the key agent with its PIN (`ssh-add -s`). One
   *  attempt per call: the PIN locks after three wrong ones. */
  securityKeyConnect: (pin: string) =>
    invoke<SecurityKeyStatusView>("security_key_connect", { pin }),
  /** Start asking the security key to sign again, after it refused. */
  securityKeyResume: () => invoke<void>("security_key_resume"),
  /** Go back to a key a binding set aside (`SetAsideView.name`). The window
   *  must start again afterwards: it signs as a different key. */
  securityKeyRestore: (name: string) => invoke<void>("security_key_restore", { name }),
  /** Withdraw a proof — your own, or (admin) the user named by `mwu1…` id. */
  securityKeyWithdraw: (user: string | null, reason: string) =>
    invoke<void>("security_key_withdraw", { user, reason }),
  attestRoots: () => invoke<AttestRootView[]>("attest_roots_view"),
  /** `cert` is a `pickFile` token for a DER certificate. */
  attestRootAdd: (cert: string, label: string) =>
    invoke<void>("attest_root_add", { cert, label }),
  attestRootRemove: (id: string) => invoke<void>("attest_root_remove", { id }),

  // -- bootstrap & wizard ---------------------------------------------------
  bootstrapStatus: () => invoke<BootstrapView>("bootstrap_status"),
  startEngine: () => invoke<StatusView>("start_engine"),
  createMesh: (meshName: string, nodeName: string, contribute: boolean) =>
    invoke<StatusView>("create_mesh", { meshName, nodeName, contribute }),
  joinToken: (token: string, nodeName: string, contribute: boolean, via: string[] = []) =>
    invoke<StatusView>("join_token", { token, nodeName, contribute, via }),
  /** `token` names the backup the person picked (`pickFile`). */
  restoreBackup: (token: string, nodeName: string, contribute: boolean) =>
    invoke<RestoreView>("restore_backup", { token, nodeName, contribute }),

  wizardPairStart: (
    nodeName: string,
    contribute: boolean,
    person: string | null,
    onEvent: (ev: WizardPairEvent) => void,
  ) => invoke<void>("wizard_pair_start", { nodeName, contribute, person, channel: channelOf(onEvent) }),
  wizardPairDecide: (accept: boolean) => invoke<void>("wizard_pair_decide", { accept }),
  /** Stop waiting to pair (DIR-6). Resolves once the wait has ended. */
  wizardPairCancel: () => invoke<void>("wizard_pair_cancel"),

  /**
   * The wizard's fifth path (L1 P3): found a real, ephemeral mesh on this
   * machine so somebody with one computer can see the product work.
   *
   * Returns as soon as the work is under way; progress and the ending
   * arrive on the channel.
   */
  demoStart: (machines: number, onEvent: (ev: DemoEvent) => void) =>
    invoke<void>("demo_start", { machines, channel: channelOf(onEvent) }),
  /** Stop the demo, remove its scratch directory, and return to the wizard. */
  demoStop: () => invoke<void>("demo_stop"),

  /**
   * One pairing code as digits, a word pair and a QR.
   *
   * Rendered in the engine (`mesh_core::qr`, `mesh_core::wordpair`) rather
   * than here, because the other machine's console is showing the same
   * three things and two implementations of a word list eventually
   * disagree — which reads to the person comparing them as an attack.
   */
  pairCode: (code: string) => invoke<PairCodeView>("pair_code_view", { code }),

  // -- views ------------------------------------------------------------------
  /**
   * *"Has anything happened?"* (**H-8**).
   *
   * One counter per topic, bumped when an op of that topic applies. The
   * cheapest call in the API — it builds no collection and takes no state
   * lock — which is what makes it affordable to ask every tick and
   * unaffordable to ask for every table.
   */
  stateVersions: () => invoke<StateVersionsView>("state_versions_view"),
  status: () => invoke<StatusView>("status_view"),
  nodes: () => invoke<NodeView[]>("nodes_view"),

  // -- the content store and the disk under it (M31 A2) ---------------------
  //
  // `contentPrune({ dryRun: true })` is what Settings calls to fill in "this
  // would free 571 MB" before anybody presses anything — the same call the
  // button makes, with one flag flipped, so the number shown and the number
  // taken can never be produced by two different code paths.
  contentStatus: () => invoke<ContentStatusView>("content_status"),
  contentPrune: (dryRun: boolean, aggressive: boolean) =>
    invoke<ContentPruneView>("content_prune", { dryRun, aggressive }),
  setContentPolicy: (policy: string, maxMb: number) =>
    invoke<ContentStatusView>("set_content_policy", { policy, maxMb }),
  disk: () => invoke<DiskView[]>("disk_view"),
  top: () => invoke<TopView[]>("top_view"),
  /** What the whole mesh adds up to (M9 §6.3). */
  totals: () => invoke<TotalsView>("totals_view"),
  /** Take the `director://` link waiting to be acted on, if any (L2 P5). */
  deepLinkTake: () => invoke<DeepLinkView | null>("deep_link_take"),
  /**
   * One page of the job table (**H-1**).
   *
   * Was an unbounded `JobView[]`: every job the mesh remembered, rebuilt
   * and re-serialised every two seconds, and past ~20 000 jobs a request
   * that simply failed. `counts` is over the whole table whatever the
   * filter, so the tab counters above a bounded list stay honest.
   */
  jobs: (
    args: {
      filter?: string | null;
      contains?: string | null;
      limit?: number | null;
      offset?: number | null;
    } = {},
  ) => invoke<JobsPageView>("jobs_view", args),
  services: () => invoke<ServiceView[]>("services_view"),
  bundles: () => invoke<BundleView[]>("bundles_view"),
  sources: () => invoke<SourceView[]>("sources_view"),
  secrets: () => invoke<SecretView[]>("secrets_view"),
  secretProviders: () => invoke<ProviderView[]>("secret_providers_view"),
  users: () => invoke<UserView[]>("users_view"),
  grants: () => invoke<GrantView[]>("grants_view"),
  audit: (args: { user: string | null; action: string | null; sinceSecs: number | null; limit: number }) =>
    invoke<AuditView[]>("audit_query", args),
  whoami: () => invoke<WhoAmIView>("whoami_view"),
  targetPreview: (selector: string) => invoke<NodeView[]>("target_preview", { selector }),
  /** The owner's decision about whether a node may be given work (admin).
   *
   *  The other half of the contribute question: `setContribute` is this
   *  peer talking about its own cores, this is the mesh deciding whether
   *  the offer is taken. Signed by the caller and enforced on every peer,
   *  so the subject cannot clear it by re-announcing. */
  nodeContributeSet: (nodeId: string, allowed: boolean, reason: string) =>
    invoke<void>("node_contribute_set", { nodeId, allowed, reason }),

  /** Isolate a **suspected** node, reversibly (admin, E2.4).
   *
   *  Not the same thing as `nodeContributeSet`, and the difference is the
   *  point: a node that is merely not contributing still gives an on-call
   *  engineer a terminal; a quarantined one gives nobody one. Reversible,
   *  unlike `nodeRevoke` — most suspicions turn out to be nothing, and
   *  revoking an identity to find that out costs a re-enrolment. */
  nodeQuarantineSet: (nodeId: string, quarantined: boolean, reason: string) =>
    invoke<void>("node_quarantine_set", { nodeId, quarantined, reason }),
  /** Assign tags to a node **in the mesh** — signed here, replicated, and
   *  kept through that machine's reboots and rebuilds.
   *
   *  Distinct from `tagsUpdate` below, which edits *this* peer's own
   *  announcement: a machine speaks for its own hardware and its own
   *  config, and an operator speaks for how their estate is divided. Only
   *  the second is somebody else's to set, which is why only this one takes
   *  a node.
   *
   *  The list is **total** — it replaces whatever that node was assigned —
   *  so a caller adding or removing one tag sends `assignedTags` from the
   *  row with the change applied, and an empty list clears the assignment. */
  nodeTagsAssign: (nodeId: string, tags: string[]) =>
    invoke<void>("node_tags_assign", { nodeId, tags }),
  /** Drain another node without stopping it (admin, Phase C E9/U9) —
   *  `meshnode nodes drain`. Its deployment instances move, each once its
   *  replacement has proved itself, until `deadlineSecs` from now (`null`
   *  for no deadline), when whatever is left stops. `lift` ends the drain.
   *  Written as the same two assigned tags the CLI writes. */
  nodeDrainSet: (nodeId: string, deadlineSecs: number | null, keepSystem: boolean, lift: boolean) =>
    invoke<void>("node_drain_set", { nodeId, deadlineSecs, keepSystem, lift }),

  // -- machine principals (E1-P4) ----------------------------------------------
  /** Every service account, with the clock each one is on. */
  serviceAccounts: () => invoke<ServiceAccountView[]>("service_accounts"),
  /** Mint one. **The returned key is the only copy** — meshware does not
   *  store it and cannot show it again, so the caller must write it out
   *  before discarding the result. */
  serviceAccountCreate: (
    name: string,
    role: string,
    nodes: string,
    services: string,
    expiresInDays: number | null,
    description: string,
  ) =>
    invoke<ServiceAccountCreated>("service_account_create", {
      name,
      role,
      nodes,
      services,
      expiresInDays,
      description,
    }),

  /** Write a freshly minted key to a file the operator picks, mode 0600.
   *  Returns the path, or `null` if they cancelled — **and cancelling
   *  loses the key**, because there is no second copy anywhere. */
  saveKeyFile: (suggested: string, key: number[]) =>
    invoke<string | null>("save_key_file", { suggested, key }),

  // -- key rotation (E2.3) -----------------------------------------------------
  /** Replace a person's key and carry their roles across. Returns how many
   *  grants moved. Bind, grant, then revoke — in that order, so they can
   *  act at every moment in between. */
  userRotateKey: (username: string, newKey: string, reason: string) =>
    invoke<number>("user_rotate_key", { username, newKey, reason }),

  // -- identity providers (E1-P5) ----------------------------------------------
  /** The issuers this mesh will believe. */
  oidcProviders: () => invoke<OidcProviderView[]>("oidc_providers"),

  // -- facts (M18 §4) -----------------------------------------------------------
  /** Every collector's answer about one node, plus what changed. */
  nodeFacts: (subject: string, historyLimit: number | null = null) =>
    invoke<NodeFactsView>("node_facts", { subject, historyLimit }),
  factCollectors: () => invoke<FactCollectorView[]>("fact_collectors_view"),
  collectorSetPaused: (name: string, paused: boolean) =>
    invoke<void>("collector_set_paused", { name, paused }),

  resultSets: (limit: number | null = null) =>
    invoke<ResultSetView[]>("result_sets_view", { limit }),

  // -- the fabric (M17 §3.12 P6) ------------------------------------------------
  /** Live sessions this peer is party to, hosted and initiated. */
  sessions: () => invoke<SessionView[]>("sessions_view"),
  /** Who may open a terminal, and where (M17 §3.4). */
  sessionPolicies: () => invoke<SessionPolicyView[]>("session_policies_view"),
  sessionPolicyPut: (args: {
    id: string;
    description: string;
    users: string;
    nodes: string;
    allow: string[];
    expiresSecs: number | null;
  }) => invoke<void>("session_policy_put", args),
  sessionPolicyRemove: (id: string) => invoke<void>("session_policy_remove", { id }),
  sessionClose: (id: number) => invoke<void>("session_close", { id }),
  /**
   * Open a terminal on `node` and stream it. Returns the id used to type
   * into it — a Tauri channel carries frames one way, so input goes back
   * through `terminalSend` rather than up the same pipe.
   */
  terminalOpen: (
    args: {
      node: string;
      command: string | null;
      /** Why — recorded in the audit log and covered by your signature. */
      reason: string;
      cols: number;
      rows: number;
      /** A container instance's job id: a shell *inside* it rather than on
       *  the node (E3) — `meshnode deployment exec`. */
      job?: string | null;
    },
    onEvent: (ev: TerminalEvent) => void,
  ) =>
    invoke<number>("terminal_open", { job: null, ...args, channel: channelOf(onEvent) }),
  /**
   * The Build tab's Run (DIR-8): the composed build line in a terminal on
   * *this* computer, hosted by Director rather than opened through the
   * mesh. `null` when the person declined the desktop's confirmation.
   * The id works with `terminalSend` / `terminalResize` / `terminalClose`.
   */
  buildRun: (
    args: { dir: string; command: string; cols: number; rows: number },
    onEvent: (ev: TerminalEvent) => void,
  ) => invoke<number | null>("build_run", { ...args, channel: channelOf(onEvent) }),
  terminalSend: (id: number, text: string) => invoke<void>("terminal_send", { id, text }),
  terminalResize: (id: number, cols: number, rows: number) =>
    invoke<void>("terminal_resize", { id, cols, rows }),
  terminalClose: (id: number) => invoke<void>("terminal_close", { id }),

  // -- this peer ----------------------------------------------------------------
  setContribute: (contribute: boolean) => invoke<StatusView>("set_contribute", { contribute }),
  tagsUpdate: (add: string[], remove: string[]) => invoke<StatusView>("tags_update", { add, remove }),
  pingNode: (node: string) => invoke<PingView>("ping_node", { node }),

  // -- runs & jobs --------------------------------------------------------------
  runCommand: (
    args: {
      command: string;
      env: [string, string][];
      /**
       * Capture each target's *answer* — the four outputs every job
       * publishes — and stream them back as rows (M19 §5.2). Off by
       * default: capture means the origin reads each target's log back, so
       * a run nobody asked a question of costs what it always did.
       */
      captureOutputs?: boolean;
      /** Extra `::meshware-output::` keys, beyond the implicit four. */
      outputs?: string[] | null;
      /** `contains(stdout, "…")` — asserted row by row on the origin. */
      expect?: string | null;
      /**
       * Run as an all-or-nothing gang of N nodes (C4 §8): every member
       * starts at the same moment, or none of them does. Mutually
       * exclusive with `fanout`, which asks the opposite question.
       */
      gang?: number | null;
      /** `pack` (default, shortest inter-task path) or `spread`. */
      gangPlacement?: string | null;
      /** Re-run the whole gang if a member dies, instead of failing it. */
      gangRestart?: boolean;
      /** Accelerators this job needs, exclusively (C5 §9). */
      gpus?: number | null;
      /**
       * Minimum memory on *each* device, in MB. A box with four 12 GB
       * cards does not satisfy 40000 — the filter is per device, never a
       * total.
       */
      vramMb?: number | null;
      /** `kind=gpu,model=~4090`; `~x` means "contains x". */
      device?: string | null;
      /**
       * Tolerate co-tenants on the devices this job is given. Opt-in,
       * because silent sharing produces OOM failures that look like
       * application bugs in somebody else's job.
       */
      gpuShare?: boolean;
      /**
       * Host ports this job needs: `http:9095`, or `http:8080!` for a
       * fixed one (N2). The allocation reaches the job as
       * MESHWARE_PORT_<LABEL>, MESHWARE_ADDR_<LABEL> and
       * MESHWARE_IP_<LABEL>.
       */
      ports?: string[] | null;
      /**
       * Register this job under NAME while it runs (N3), so `NAME.mesh`
       * resolves to it. Needs a port: a name with no address resolves to
       * nothing.
       */
      register?: string | null;
      /**
       * Volumes this job claims (N5): `data`, `data:ro`, or
       * `data:/mnt/data:rw` to say where a container mounts it. A claim is
       * a filter — only a node that lends the volume, and can lend it in
       * that mode right now, is a candidate.
       */
      volumes?: string[] | null;
    } & DispatchArgs,
    onEvent: (ev: RunEvent) => void,
  ) => invoke<void>("run_command", { ...args, channel: channelOf(onEvent) }),
  runService: (
    args: {
      name: string;
      inputs: [string, string][];
      /** As `runCommand` — a script answers with rows the same way a command does. */
      captureOutputs?: boolean;
      outputs?: string[] | null;
      expect?: string | null;
      /** As `runCommand`. A service is the shape that gets a contract and
       *  a cache, so it is the shape GPU work wants. */
      /**
       * Run as an all-or-nothing gang of N nodes (C4 §8, R1 P4). A gang is
       * requested through reserved environment keys, so a saved script can
       * be one — which is the shape gang work actually takes.
       */
      gang?: number | null;
      gangPlacement?: string | null;
      gangRestart?: boolean;
      gpus?: number | null;
      vramMb?: number | null;
      device?: string | null;
      gpuShare?: boolean;
      /**
       * Host ports this job needs: `http:9095`, or `http:8080!` for a
       * fixed one (N2). The allocation reaches the job as
       * MESHWARE_PORT_<LABEL>, MESHWARE_ADDR_<LABEL> and
       * MESHWARE_IP_<LABEL>.
       */
      ports?: string[] | null;
      /**
       * Register this job under NAME while it runs (N3), so `NAME.mesh`
       * resolves to it. Needs a port: a name with no address resolves to
       * nothing.
       */
      register?: string | null;
      /**
       * Volumes this job claims (N5): `data`, `data:ro`, or
       * `data:/mnt/data:rw` to say where a container mounts it. A claim is
       * a filter — only a node that lends the volume, and can lend it in
       * that mode right now, is a candidate.
       */
      volumes?: string[] | null;
    } & DispatchArgs,
    onEvent: (ev: RunEvent) => void,
  ) => invoke<void>("run_service", { ...args, channel: channelOf(onEvent) }),
  jobLogs: (jobId: string, follow: boolean, onEvent: (ev: RunEvent) => void) =>
    invoke<void>("job_logs", { jobId, follow, channel: channelOf(onEvent) }),
  /**
   * Stop following a job's logs (**H-10**).
   *
   * A follow runs until the job ends, so a window that merely stopped
   * *reading* left the engine streaming into nothing. Idempotent — call it
   * on every deselect and on unmount without tracking whether a stream was
   * open.
   */
  jobLogsStop: (jobId: string) => invoke<void>("job_logs_stop", { jobId }),
  /**
   * Cancel a job. The daemon reaches the executor when it can and falls
   * back to a signed force-cancel when it cannot — resolves with its
   * account of which happened.
   */
  jobCancel: (jobId: string) => invoke<string>("job_cancel", { jobId }),
  /**
   * Forget every job that has already finished — mesh-wide, permanently.
   * Admin-only; running and queued work is untouched. Resolves with the
   * daemon's own count of what went.
   */
  jobsClear: () => invoke<string>("jobs_clear"),

  // -- parallel compute (MASTERPLAN M9 §5.4) ------------------------------------
  /** One command per item, spread across the mesh by pull-based work stealing. */
  parallelRun: (
    args: {
      command: string;
      /** A Library script to run per item, instead of `command`. */
      service: string | null;
      inputs: [string, string][];
      /** Which of the script's inputs receives each item. */
      itemInput: string | null;
      items: string[];
      on: string | null;
      env: [string, string][];
      timeoutSecs: number | null;
      cpus: number | null;
      memMb: number | null;
      diskMb: number | null;
      /** null = adaptive chunking, which is the default and the recommendation. */
      chunk: number | null;
      /**
       * Keep each item's output on the node that produced it and pass back
       * only a reference (M9 §5.5). This is what stops intermediate data
       * funnelling through the node that launched the job.
       */
      keepOutput: boolean;
      /** Merge command run once over every output, from $MESHWARE_INPUTS. */
      reduce: string | null;
      /** Where the reduce runs; null = wherever most of the output is. */
      reduceOn: string | null;
      /** Resolve the workers and report what would run, dispatching nothing. */
      dryRun?: boolean;
    },
    onEvent: (ev: ParallelEvent) => void,
  ) =>
    invoke<void>("parallel_run", {
      dryRun: false,
      ...args,
      channel: channelOf(onEvent),
    }),

  // -- library: bundles, services, sources ------------------------------------
  /** `token` names a picked folder or a dropped file (SEC-14). */
  pushBundle: (token: string) => invoke<PushResultView>("push_bundle", { token }),
  /** Files in a bundle, typed for the picker (manifest only — cheap). */
  bundleFiles: (bundleId: string) => invoke<BundleFileView[]>("bundle_files", { bundleId }),
  serviceCreate: (args: {
    name: string;
    runtime: string;
    bundleId: string;
    workingDir: string;
    entrypoint: string;
    contract: string | null;
    env: [string, string][];
    description: string;
    tags: string[];
    /** Container only (N4): override the image's ENTRYPOINT, as Nomad's
     *  `config { command = … }` does. */
    command?: string | null;
    /** Container only: arguments for `command`, in order. */
    args?: string[] | null;
  }) => invoke<number>("service_create", args),
  serviceRollback: (name: string, version: number) => invoke<void>("service_rollback", { name, version }),


  // -- deployments: work that is supposed to stay up (C2) -------------------
  deployments: () => invoke<DeploymentView[]>("deployments_view"),
  deploymentStatus: (name: string) =>
    invoke<DeploymentStatusView>("deployment_status", { name }),
  deploymentRollout: (name: string, version: number) =>
    invoke<void>("deployment_rollout", { name, version }),
  /** One service version back, or — with `target` — back to a whole kept
   *  version by generation or tag (E1): `deployment rollback --generation`. */
  deploymentRollback: (name: string, to?: number, target?: { generation?: number; tag?: string }) =>
    invoke<void>("deployment_rollback", {
      name,
      to: to ?? null,
      generation: target?.generation ?? null,
      tag: target?.tag ?? null,
    }),
  /** The versions a deployment keeps, newest first, and its tags (E1, U2). */
  deploymentHistory: (name: string) =>
    invoke<DeploymentHistoryView>("deployment_history", { name }),
  /** Name a version — the running one by default — or remove a name (E1). */
  deploymentTag: (
    name: string,
    tag: string,
    opts?: { generation?: number; description?: string; remove?: boolean },
  ) =>
    invoke<void>("deployment_tag", {
      name,
      tag,
      generation: opts?.generation ?? null,
      description: opts?.description ?? null,
      remove: opts?.remove ?? false,
    }),
  /** Let a held canary set cut over (E2) — the generation the screen shows. */
  deploymentPromote: (name: string, generation: number) =>
    invoke<void>("deployment_promote", { name, generation }),
  deploymentSetPaused: (name: string, paused: boolean) =>
    invoke<void>("deployment_set_paused", { name, paused }),
  deploymentRemove: (name: string) => invoke<void>("deployment_remove", { name }),
  /** Resize a fixed deployment; restarts none of the instances that stay (E3). */
  deploymentScale: (name: string, count: number) =>
    invoke<void>("deployment_scale", { name, count }),
  /** Replace every instance in batches, or one slot's instance (E3). */
  deploymentRestart: (name: string, slot?: string) =>
    invoke<void>("deployment_restart", { name, slot: slot ?? null }),
  /** One instance slot up close — its timeline, health and live use (E3, U1).
   *  `meshnode deployment instance`. */
  deploymentInstance: (name: string, slot: string) =>
    invoke<DeploymentInstanceDetailView>("deployment_instance", { name, slot }),
  /** One deployment with its definition, so the editor can load it. */
  deployment: (name: string) =>
    invoke<DeploymentView>("deployment_detail", { name }),
  /** Publish a definition, or — with `dryRun` — ask what it would do.
   *  A dry run answers with the check; a real put answers with `null`. */
  deploymentPut: (source: string, dryRun = false) =>
    invoke<DeploymentCheckView | null>("deployment_put", { source, dryRun }),
  /** Parse a definition with no mesh and say what it would do (E4) —
   *  `meshnode deployment validate`. Rejects with the parser's message. */
  deploymentValidate: (source: string) =>
    invoke<[string, string][]>("deployment_validate", { source }),
  /** The commented starting definition (E4, U14) — `deployment init`. */
  deploymentExample: (name = "web", image = "docker.io/library/nginx:alpine") =>
    invoke<string>("deployment_example", { name, image }),
  /** The JSON Schema for definition files (E4) — `deployment schema`. */
  deploymentSchema: () => invoke<string>("deployment_schema"),

  /** Read a picked or dropped definition file as text. Bounded at 1 MB
   *  and UTF-8 only — a definition is a page of TOML. */
  readSpecFile: (token: string) => invoke<string>("read_spec_file", { token }),

  // -- the service registry: what is running, and where (N3) ----------------
  //
  // Answered from local replicated state — every node holds the whole
  // catalogue, so Director's embedded node gives the same answer any other
  // node would and there is no leader to ask.
  registry: () => invoke<RegistryRowView[]>("registry_view"),
  registryService: (service: string) =>
    invoke<RegistryInstanceView[]>("registry_service", { service }),
  /** What this node's DNS responder is doing. */
  dnsStatus: () => invoke<DnsStatusViewT>("dns_status"),

  // -- ports: the one resource that is exclusive, not divisible (N2) --------
  //
  // This node's ledger. Node-local by construction until the service
  // registry lands (N3), and the panel says so rather than implying a
  // mesh-wide answer.
  ports: () => invoke<PortLedgerViewT>("ports_view"),

  // -- runtimes: what each machine can actually run (T4) --------------------
  //
  // Two halves with two confidences, and the panel keeps them apart: this
  // node's own probes in full, because it ran them; every other node from
  // the `cap.rt` it announced, because a probe is node-local and a
  // replicated diagnosis would describe a machine as it was at boot.
  //
  // The second half is exactly what placement reads, so this panel and the
  // scheduler cannot disagree about who can run what.
  runtimes: () => invoke<RuntimesViewT>("runtimes_view"),

  // -- placement: what runs where, machine by machine (Phase C, U8) ---------
  /** `meshnode deployment placement` — every machine with the instances on
   *  it and what each asked for. */
  placementView: () => invoke<PlacementNodeView[]>("placement_view"),

  // -- volumes: the other exclusive resource, and the durable one (N5) ------
  //
  // Node-local for the same reason the port ledger is, and more strongly: a
  // volume *is* a directory on one machine, so the node holding the bytes
  // is the only one that can arbitrate access to them.
  volumes: () => invoke<VolumeRowView[]>("volumes_view"),

  /** Provision one through this node's `[runtime] volume_plugin` (N5 P2).
   *  `params` is `key=value` lines, the shape a plugin's own
   *  documentation is written in. */
  volumeCreate: (
    name: string,
    capacityMb?: number | null,
    params?: string | null,
    readOnly?: boolean,
  ) =>
    invoke<VolumeRowView>("volume_create", {
      name,
      capacityMb: capacityMb ?? null,
      params: params ?? null,
      readOnly: readOnly ?? false,
    }),

  /** Stop offering a provisioned volume, and ask the plugin to remove it.
   *  Never touches a `[[volume]]` block, and never overrides a live
   *  lease — `force` only forgives a plugin that refused. */
  volumeRemove: (name: string, force = false) =>
    invoke<string>("volume_remove", { name, force }),

  // -- the mesh-wide shed: stop taking work, everywhere at once (N6) --------
  //
  // The one capability gate in meshware that fails *open* — a node too old
  // to read the record keeps accepting — so `warning` is not decoration and
  // the panel shows it wherever the switches are shown.
  shed: () => invoke<ShedView>("shed_view"),
  /** Both switches, absolute. Read the current record first and send the
   *  state you want to be in: two admins flipping different halves from
   *  stale views would otherwise write a record neither asked for. */
  shedSet: (submissions: boolean, reconcile: boolean, reason?: string) =>
    invoke<ShedView>("shed_set", { submissions, reconcile, reason: reason ?? null }),

  // -- schedules: work that runs itself, on a clock (N1 P5) -----------------
  schedules: () => invoke<ServiceScheduleView[]>("schedules_view"),
  schedule: (name: string) =>
    invoke<ServiceScheduleView>("schedule_detail", { name }),
  /** Publish a definition, or — with `dryRun` — ask what it would do.
   *  A dry run answers with the check; a real put answers with `null`. */
  schedulePut: (source: string, dryRun = false) =>
    invoke<ServiceScheduleCheckView | null>("schedule_put", {
      source,
      dryRun,
    }),
  scheduleSetPaused: (name: string, paused: boolean) =>
    invoke<void>("schedule_set_paused", { name, paused }),
  scheduleRemove: (name: string) => invoke<void>("schedule_remove", { name }),

  // -- gangs: all-or-nothing reservations (C4) -------------------------------
  //
  // `all` includes gangs that have finished. Off by default, because the
  // question this noun exists for is about what is holding resources now.
  gangs: (all = false) => invoke<GangRowView[]>("gangs_view", { all }),
  /** Every accelerator in the mesh, from replicated state (C5 §9.1). */
  devices: () => invoke<DeviceView[]>("devices_view"),

  // -- C8 legibility: the scheduler shows its work (§12) ---------------------
  //
  // meshware has computed a weighted score over eight terms for every
  // candidate on every placement since M5, and thrown all of it away except
  // the winner. This is the same arithmetic, kept.
  //
  // `compare` names another job whose action key is diffed against this
  // one's — what turns "this was a cache miss" into "this was a miss
  // because `env` changed".
  jobExplain: (job: string, compare?: string) =>
    invoke<JobExplainView>("job_explain", { job, compare: compare ?? null }),
  /**
   * What-if: where this would land, when it would finish, whose work it
   * delays — submitting nothing. `run --dry-run` answers the other
   * question: what it would touch.
   */
  plan: (
    on?: string,
    service?: string,
    env: [string, string][] = [],
    fanout?: number,
  ) =>
    invoke<PlanView>("plan_view", {
      on: on ?? null,
      service: service ?? null,
      env,
      fanout: fanout ?? null,
    }),

  // -- C7 the action cache (§11) --------------------------------------------
  //
  // A hit is visible or it is untrustworthy: "it didn't run" and "it broke"
  // are indistinguishable unless the product says which.
  cacheStats: () => invoke<CacheStatsView>("cache_stats"),
  /**
   * The one shell line that runs a build through the mesh, composed by the
   * engine.
   *
   * Not built here on purpose: the wrapper binary is `current_exe()` and
   * the mesh home is Director's own, and neither is knowable from the
   * frontend. Guessing at them is exactly how the old printed line came to
   * name a `meshnode` that is not installed, pointed at a home with no
   * daemon in it.
   */
  farmBuildLine: (dir: string, command: string) =>
    invoke<FarmBuildLine>("farm_build_line", { dir, command }),

  /** Forget entries: everything, one `act-…` key, or one service's. */
  cachePurge: (target?: string) =>
    invoke<string>("cache_purge", { target: target ?? null }),

  // -- arbitration: the queue (C1) -----------------------------------------
  /** This node's pending queue, in the order it will actually run them. */
  queue: () => invoke<QueueRowView[]>("queue_view"),

  // -- M31: how long we keep it, and who else gets a copy -----------------------
  //
  // Two questions, one paragraph. `retention` is every mesh's business;
  // `sinks` is almost nobody's, and the UI keeps it that way (§18
  // decision 01) — one row saying "not configured" and nothing anywhere else.

  /** The retention windows, and where they settle the op-log (A1). */
  retention: () => invoke<RetentionView>("retention_view"),
  /** Sign and publish edited windows, as `table=window` pairs. Only the
   *  tables named are changed — silence about a table is not a reset,
   *  which matters most for `audit`. */
  retentionPut: (windows: string[]) => invoke<RetentionView>("retention_put", { windows }),
  /** Withdraw the policy, back to the shipped (finite) defaults. */
  retentionReset: () => invoke<RetentionView>("retention_reset"),

  /** What this node is refusing, and what is churning (M31 A3).
   *
   *  Empty on a healthy mesh — the screen shows nothing when it is. It
   *  answers the other half of retention's question: the window says where
   *  the log settles, this says whether anything is trying to outrun it. */
  backpressure: () => invoke<BackpressureView>("backpressure_view"),

  /** Every sink, with lag, throughput and the destination's last words. */
  sinks: () => invoke<SinkView[]>("sinks_view"),
  /** The destination list and what each one gets you *afterwards* —
   *  rendered from the capability triple on the enum, so the picker cannot
   *  promise a read the code cannot do (§14.4). */
  sinkKinds: () => invoke<SinkKindView[]>("sink_kinds"),
  /** Create or replace. `credential` is a secret **name**; Director turns
   *  it into a `{{secret:…}}` reference and never sees the value (C6). */
  sinkPut: (draft: SinkDraft) => invoke<void>("sink_put", { draft }),
  sinkRemove: (name: string) => invoke<void>("sink_remove", { name }),
  /** POST one synthetic row from the **unsaved form**, so a wrong token is
   *  caught in the dialog rather than by a lag graph two hours later.
   *  Resolves with the destination's own words on success and rejects with
   *  them on failure — either way, verbatim. */
  sinkTest: (draft: SinkDraft) => invoke<string>("sink_test", { draft }),
  /** Start exporting again after a restore held every sink (§19 Q5). */
  sinksResume: () => invoke<void>("sinks_resume"),

  /** Browse what ran, across both tiers — `history.redb` for the recent
   *  rows and Parquet for the rest (§14.2). The page says which tier
   *  answered; it never asks a sink, and that is the fence. */
  historyJobs: (q: {
    jobState?: string;
    sinceMicros?: string;
    untilMicros?: string;
    contains?: string;
    limit?: number;
    offset?: number;
  }) => invoke<HistoryJobPageView>("history_jobs", q),
  /** One window of one job's output, from wherever a sink put it (§14.3).
   *
   *  Named by job, never by key: the daemon holds the locator, so this
   *  cannot be aimed at an arbitrary row in somebody's warehouse. Windowed
   *  from the end because the reason anyone opens an old log is at the
   *  bottom of it, and `fromEnd: 0` — the tail — is what the daemon audits
   *  as an *open*. Scrolling back costs no audit entry. */
  historyReplay: (
    job: string,
    fromEnd = "0",
    limit = 500,
    search = "",
  ) => invoke<PayloadWindowView>("history_replay", { job, fromEnd, limit, search }),

  serviceRemove: (name: string) => invoke<void>("service_remove", { name }),
  sourceAdd: (args: { name: string; url: string; branch: string; intervalSecs: number; bump: string[] }) =>
    invoke<void>("source_add", args),
  sourceRemove: (name: string) => invoke<void>("source_remove", { name }),
  sourceSync: (name: string) => invoke<SyncResultView>("source_sync", { name }),

  // -- secrets ------------------------------------------------------------------
  secretSet: (name: string, value: string) => invoke<SecretSetView>("secret_set", { name, value }),
  secretRemove: (name: string) => invoke<void>("secret_remove", { name }),

  // -- access -------------------------------------------------------------------
  userAdd: (args: {
    username: string;
    key: string;
    role: string;
    nodes: string;
    services: string;
    expiresSecs: number | null;
  }) => invoke<void>("user_add", args),
  userRevoke: (user: string, reason: string) => invoke<void>("user_revoke", { user, reason }),

  // -- enrolment (M23 §3.2) -----------------------------------------------------
  //
  // People enrol the way nodes pair: they ask, both sides see the same
  // short code, and a human with authority says yes. `enrolApprove` is the
  // existing UserAdd + GrantAdd pair — there is no new authorization path.
  enrolments: () => invoke<EnrolmentView[]>("enrolments_view"),
  enrolApprove: (args: {
    user: string;
    username: string;
    role: string;
    nodes: string;
    services: string;
    expiresSecs: number | null;
  }) => invoke<void>("enrol_approve", args),
  enrolDecline: (user: string, reason: string) => invoke<void>("enrol_decline", { user, reason }),
  /** The same person at a new machine: bind the key to their existing
   *  username and copy their grants. Resolves to how many were copied. */
  enrolAttach: (user: string, username: string) =>
    invoke<number>("enrol_attach", { user, username }),
  /** Director's own "Request access": returns the code to show an admin. */
  enrolRequest: (username: string) => invoke<string>("enrol_request", { username }),
  grantAdd: (args: { user: string; role: string; nodes: string; services: string; expiresSecs: number | null }) =>
    invoke<string>("grant_add", args),
  grantRevoke: (grantId: string) => invoke<void>("grant_revoke", { grantId }),

  // -- distribution (M24) --------------------------------------------------------
  //
  // Definitions replicate through the op-log automatically; bundle content
  // does not, and binaries never do. These three verbs are that line.
  definitions: () => invoke<DefinitionView[]>("definitions_view"),
  /** Ask a set of nodes to fetch a bundle *now* (§3.2). */
  bundlePrefetch: (bundle: string, selector: string, onEvent: (ev: PrefetchEvent) => void) =>
    invoke<void>("bundle_prefetch", { bundle, selector, channel: channelOf(onEvent) }),
  /** Widen (or narrow) the ring a collector runs on — a deployment (§3.3). */
  collectorWiden: (name: string, selector: string) =>
    invoke<void>("collector_widen", { name, selector }),
  /** Roll a release across a ring, serially, health-gated (§4). */
  upgradeFleet: (
    selector: string,
    version: string,
    batch: number,
    movePending: boolean,
    force: boolean,
    onEvent: (ev: UpgradeEvent) => void,
  ) =>
    invoke<void>("upgrade_fleet", {
      selector,
      version,
      batch,
      movePending,
      force,
      channel: channelOf(onEvent),
    }),

  /** What this fleet verifies releases against (§9). */
  releaseTrust: () => invoke<ReleaseTrustView>("release_trust"),
  /** Trust a signing key mesh-wide: a .pub file path or the bare RW… line. */
  /** `source` is pasted key text; `file` a picked `.pub` file's token. */
  releaseKeyTrust: (source: string, file: string | null, label: string | null) =>
    invoke<void>("release_key_trust", { source, file, label }),
  /** Withdraw trust in a signing key. */
  releaseKeyRevoke: (pubkey: string) => invoke<void>("release_key_revoke", { pubkey }),
  /** Publish a security key's co-signature (Y6): `sig` is a picked
   *  `ssh-keygen -Y sign` file, `digest` the binary's, from `releaseTrust`. */
  releaseCosign: (sig: string, digest: string) => invoke<string>("release_cosign", { sig, digest }),
  /** Versions this mesh has published, newest first (§9). */
  releaseVersions: () => invoke<string[]>("release_versions"),
  /** Builds in a folder, each with the reason it cannot be published, if any. */
  /** Builds in a picked folder (which is then remembered), or — with
   *  `null` — in the remembered one. */
  releaseCandidates: (token: string | null) =>
    invoke<BuildCandidateView[]>("release_candidates", { token }),
  /** The remembered build folder, to show. */
  releaseBuildDir: () => invoke<string | null>("release_build_dir"),
  releaseBuildDirForget: () => invoke<void>("release_build_dir_forget"),
  /** Publish one build into the mesh; nodes fetch it from each other after this. */
  /** `token` is a build's own, from `releaseCandidates`. */
  releasePublish: (token: string, version: string) =>
    invoke<string>("release_publish", { token, version }),

  // -- membership ---------------------------------------------------------------
  nodeRevoke: (nodeId: string, reason: string) => invoke<void>("node_revoke", { nodeId, reason }),
  /**
   * Repair what the mesh records as a node's addresses (founder only).
   * A first-contact address, not a source of truth: the node overwrites it
   * with its own announcement as soon as the two can talk again.
   */
  nodeAddrRepair: (nodeId: string, addrs: string[]) =>
    invoke<void>("node_addr_repair", { nodeId, addrs }),
  inviteCreate: (ttlSecs: number, uses: number, tags: string[], addrs: string[] = []) =>
    invoke<string>("invite_create", { ttlSecs, uses, tags, addrs }),
  /** Every invite the mesh knows about, spent and revoked ones included. */
  invites: () => invoke<InviteView[]>("invites_list"),
  /** Take an invite back, by id or by the token itself; its token stops
   *  working everywhere. Machines that already joined with it stay. */
  inviteRevoke: (invite: string) => invoke<void>("invite_revoke", { invite }),
  discoverCandidates: (waitMs: number) => invoke<CandidateView[]>("discover_candidates", { waitMs }),
  /** The background listener for machines waiting to pair (Access's join
   *  queue). Started once, read on every tick, stopped from Settings. */
  lanWatchStart: () => invoke<void>("lan_watch_start"),
  lanWatchStop: () => invoke<void>("lan_watch_stop"),
  lanWatch: () => invoke<LanWatchView>("lan_watch_view"),
  /** Dial a candidate at each of its addresses, best first, until one answers. */
  pairApproveStart: (addrs: string[], tags: string[], onEvent: (ev: ApproveEvent) => void) =>
    invoke<void>("pair_approve_start", { addrs, tags, channel: channelOf(onEvent) }),
  pairApproveDecide: (session: number, accept: boolean) =>
    invoke<void>("pair_approve_decide", { session, accept }),

  // -- day-2 --------------------------------------------------------------------
  doctor: () => invoke<DoctorCheckView[]>("doctor_run"),
  /** `meshnode doctor --fix`: the safe fixes, then the checks run again. */
  doctorFix: () => invoke<DoctorFixReport>("doctor_fix"),
  configView: () => invoke<string>("config_view"),
  /** Write a restorable backup to a file the person picks (chooser in Rust,
   *  S3 — it holds this node's private keys). `null` if they cancelled. */
  backupWrite: (suggested: string) =>
    invoke<BackupView | null>("backup_write", { suggested }),
  /** Write a composed mesh snapshot (L2 P2). Base64 because Tauri sends a
   *  byte array as JSON numbers — four bytes of transport per byte of PNG.
   *  Chooser in Rust (S3); `null` if they cancelled. */
  snapshotSave: (suggested: string, pngBase64: string) =>
    invoke<string | null>("snapshot_save", { suggested, pngBase64 }),
  drain: (movePending: boolean, onEvent: (ev: DrainEvent) => void) =>
    invoke<void>("drain", { movePending, channel: channelOf(onEvent) }),
  /** `meshnode drain --until` / `--for`: refuse new work until `untilSecs`
   *  (Unix seconds), keep what is running, rejoin on your own — the engine
   *  stays up. `null` lifts a timed drain early. Resolves to the engine's
   *  own sentence about it. */
  drainTimed: (untilSecs: number | null, reason: string) =>
    invoke<string>("drain_timed", { untilSecs, reason }),

  // -- add machines / upgrade over SSH / release channel (deploy.rs, channel.rs) ---
  /** What a deploy would do: expanded hosts, unreadable entries, binary sources. */
  /** `binaryPath` is a picker token, never a path. */
  deployPreview: (targets: string[], binaryPath: string | null, port: number) =>
    invoke<DeployPreviewView>("deploy_preview", { targets, binaryPath, port }),
  /** Start a deploy; events stream in, the id addresses decisions and cancel. */
  deployStart: (draft: DeployDraft, onEvent: (ev: DeployEvent) => void) =>
    invoke<number>("deploy_start", { draft, channel: channelOf(onEvent) }),
  /** The person's verdict on one machine's pairing code. */
  deployDecide: (runId: number, host: string, accept: boolean) =>
    invoke<void>("deploy_decide", { runId, host, accept }),
  deployCancel: (runId: number) => invoke<void>("deploy_cancel", { runId }),
  /** Push a verified binary onto nodes over SSH and restart them. */
  upgradeSshStart: (draft: SshUpgradeDraft, onEvent: (ev: DeployEvent) => void) =>
    invoke<number>("upgrade_ssh_start", { draft, channel: channelOf(onEvent) }),
  /** The fleet against the mesh's releases and the release channel. */
  releaseCheck: () => invoke<ReleaseCheckView>("release_check"),
  /** Download a release per architecture, verify it, publish it into the mesh. */
  releaseFetch: (version: string, arches: string[], onEvent: (ev: ReleaseFetchEvent) => void) =>
    invoke<void>("release_fetch", { version, arches, channel: channelOf(onEvent) }),

  // -- the OS keychain (credentials.rs) ------------------------------------------
  /** What is remembered for an SSH user name, or null. */
  /** **Which** secrets are remembered for this user, never the secrets (S3).
   *  The form leaves those inputs blank and sends `fromKeychain: true`; the
   *  backend reads the OS store itself when the deploy needs them. */
  credentialGet: (user: string) =>
    invoke<RememberedCredentialView | null>("credential_get", { user }),
  /** Remember `auth` for exactly the machines a run reached with it — the
   *  run's own card keys, from its `plan` event. One record per account on
   *  one machine, so a password is never offered to a machine it was not
   *  typed for. */
  credentialPut: (auth: DeployAuthDraft, hosts: string[]) =>
    invoke<void>("credential_put", { auth, hosts }),
  credentialForget: (user: string) => invoke<void>("credential_forget", { user }),
  /** User names with a remembered record (names only). */
  credentialUsers: () => invoke<string[]>("credential_users"),

  // -- Director itself (selfupdate.rs, desktop.rs) --------------------------------
  directorUpdateCheck: () => invoke<DirectorUpdateView>("director_update_check"),
  directorUpdateInstall: (onEvent: (ev: DirectorUpdateEvent) => void) =>
    invoke<void>("director_update_install", { channel: channelOf(onEvent) }),
  autostartGet: () => invoke<boolean>("autostart_get"),
  autostartSet: (enabled: boolean) => invoke<void>("autostart_set", { enabled }),
  /** Put text on the OS clipboard — reliable where `navigator.clipboard` is not. */
  copyText: (text: string) => invoke<void>("copy_text", { text }),
  /** Open an http(s) address in the desktop's browser — the Addresses page's
   *  "Open" on a copy — or a `mailto:` in the mail client (the help menu's
   *  "Contact support"). Anything else is refused on the Rust side. */
  openUrl: (url: string) => invoke<void>("open_url", { url }),
  /** This build, its licence and its plan (`about.rs`, UX-7). Engine-free. */
  about: () => invoke<AboutView>("about_view"),
  directorLogDir: () => invoke<string>("director_log_dir"),
  /** Show Director's log folder in the file manager (DIR-11). */
  openLogDir: () => invoke<void>("open_log_dir"),
  /** Write a frontend error into Director's log (DIR-11, `lib/crashlog`). */
  frontendLog: (kind: string, message: string, stack: string | null) =>
    invoke<void>("frontend_log", { kind, message, stack }),
  /** Is the tray icon actually being drawn? See `glance.rs`. */
  trayStatus: () => invoke<TrayStatusView>("tray_status"),

  // -- native pickers (opened on the Rust side) -----------------------------------
  //
  // All three go through `onePicker`, and the reason is a bug that cost a
  // whole session: a desktop whose native chooser did not open left the
  // plugin's `save`/`open` command — an `async fn` blocking on a channel
  // the OS would never answer — parked on a Tauri async-runtime worker for
  // the life of the process. A person clicking a dead button clicks it
  // again, and every click burned another worker until nothing Director
  // asked its own engine could be scheduled: Save stuck on "Signing…",
  // scripts that would not run, and a restart the only way out.
  //
  // The chooser itself is fixed where it broke (the workspace pins the
  // plugin to the XDG portal rather than `rfd`'s GTK3 backend). This is
  // the second lock on the same door: whatever the desktop does, at most
  // one native dialog is ever outstanding, so a picker that never answers
  // costs one worker and not the app.
  /**
   * Save a per-item run's outputs into a directory (R3 P1).
   *
   * Naming is the engine's `outputs::fetch_filename`, the same one
   * `meshnode map --out` uses, so a file saved from here and a file saved
   * from the CLI are named identically.
   */
  /**
   * What a script in a bundle says about itself (R2 P4) — its `#meshware:`
   * header, or what can be read out of the flags it parses. The same parser
   * the CLI uses, so a script added here and one added with `meshnode add`
   * register identically.
   */
  scriptInspect: (bundleId: string, path: string) =>
    invoke<ScriptSpecView>("script_inspect", { bundleId, path }),
  /**
   * Write a script's `.definitions` file back to the folder it came from.
   * Returns the path written. The folder is the one this Director added
   * `bundleId` from — the window never names one (SEC-14).
   */
  scriptDefinitionsSave: (bundleId: string, entry: string, definitions: string) =>
    invoke<string>("script_definitions_save", { bundleId, entry, definitions }),
  /**
   * Compile a definitions file being edited, so the argument rows and the
   * JSON stay two views of one thing. Same parser that reads it off disk.
   */
  scriptDefinitionsPreview: (definitions: string) =>
    invoke<ScriptSpecView>("script_definitions_preview", { definitions }),
  /**
   * Remember how a script was just run, as the way it runs from now on.
   *
   * `run` is the settings block (`mode`, `on`, `spread`, `gang`, … — see
   * `RunDefaults` in `mesh-core/src/script_spec.rs`, which is the one
   * definition of the shape). `argDefaults` are the declared inputs, which
   * become the arguments' own defaults rather than living in the run
   * block: an argument's default is an argument's business.
   *
   * The `.definitions` file on disk is written too when this Director
   * added the script's folder itself; otherwise only the contract is,
   * which is the half that replicates and the half that matters.
   */
  serviceSaveRunDefaults: (
    name: string,
    run: Record<string, unknown>,
    argDefaults: [string, string][],
  ) =>
    invoke<RunDefaultsSavedView>("service_save_run_defaults", {
      name,
      run,
      argDefaults,
    }),

  parallelSaveOutputs: (
    token: string,
    outputs: { item: string; id: string; holder: string }[],
    job: string | null,
  ) => invoke<number>("parallel_save_outputs", { token, outputs, job }),

  // -- streamed saves (src-tauri/src/filesave.rs) --------------------------------
  //
  // For a document too large to hand over as one argument. `fileSaveBegin`
  // opens the chooser and answers with a handle (or `null` for a cancel);
  // the caller writes it a piece at a time and closes it. See
  // `lib/save.ts`, which is the only thing that should call these four
  // directly.
  //
  // It goes through `onePicker` for the same reason the three below do: the
  // chooser is native, and two of them outstanding is one dialog answering
  // the wrong caller.
  fileSaveBegin: (
    title: string,
    suggested: string,
    filterName: string | null = null,
    extensions: string[] | null = null,
  ): Promise<number | null> =>
    onePicker(() =>
      invoke<number | null>("file_save_begin", { title, suggested, filterName, extensions }),
    ),
  /** Append a chunk; answers with the total bytes written so far. */
  fileSaveWrite: (id: number, chunk: string) => invoke<number>("file_save_write", { id, chunk }),
  /** Flush and close; answers with the path written. */
  fileSaveFinish: (id: number) => invoke<string>("file_save_finish", { id }),
  /** Give up, and take the partial file with it. */
  fileSaveAbort: (id: number) => invoke<void>("file_save_abort", { id }),

  // The dialog opens on the Rust side, and what comes back is a token for
  // the pick plus the path to *show* (SEC-14). A path the window holds is
  // never sent back: every command that reads or writes a file the person
  // chose takes the token.
  pickDirectory: (title: string): Promise<PickedView | null> =>
    onePicker(() => invoke<PickedView | null>("pick_directory", { title })),
  pickFile: (title: string, extensions?: string[]): Promise<PickedView | null> =>
    onePicker(() =>
      invoke<PickedView | null>("pick_file", { title, extensions: extensions ?? null }),
    ),
  /** Files dropped on the window, as picks. Returns the unsubscribe. */
  onDropped: (handler: (picks: PickedView[]) => void): Promise<() => void> =>
    listen<PickedView[]>("mw-dropped", (ev) => handler(ev.payload)),
};

/** Is a native dialog outstanding? */
let pickerOpen = false;

/**
 * Run one native picker at a time.
 *
 * A request made while a chooser is already open answers `null` — the same
 * "nothing was chosen here" a cancel gives — rather than opening a second
 * dialog or, worse, handing this caller the *other* dialog's answer: a
 * directory where a file was asked for. The first dialog still resolves
 * into the field that opened it.
 */
async function onePicker<T>(open: () => Promise<T | null>): Promise<T | null> {
  if (pickerOpen) return null;
  pickerOpen = true;
  try {
    return await open();
  } finally {
    pickerOpen = false;
  }
}

export function errText(e: unknown): string {
  const text = typeof e === "string" ? e : e instanceof Error ? e.message : String(e);
  // The marker is for the window, not for a person.
  return text.startsWith(KEY_PAUSED) ? text.slice(KEY_PAUSED.length) : text;
}

/** "30d" / "12h" / "45m" / "90s" / "600" → seconds; null for empty. Throws on garbage. */
export function parseDuration(s: string): number | null {
  const t = s.trim().toLowerCase();
  if (!t) return null;
  const m = /^(\d+)\s*(s|m|h|d|w)?$/.exec(t);
  if (!m) throw new Error(`invalid duration "${s}" (try 30d, 12h, 45m, 90s)`);
  const n = Number(m[1]);
  const unit = m[2] ?? "s";
  const mult = { s: 1, m: 60, h: 3600, d: 86400, w: 604800 }[unit] ?? 1;
  return n * mult;
}

export function fmtDuration(secs: number): string {
  if (secs % 86400 === 0 && secs >= 86400) return `${secs / 86400}d`;
  if (secs % 3600 === 0 && secs >= 3600) return `${secs / 3600}h`;
  if (secs % 60 === 0 && secs >= 60) return `${secs / 60}m`;
  return `${secs}s`;
}

/**
 * A duration rounded to the unit a person would say it in.
 *
 * `fmtDuration` above only names a unit on an exact division, which is
 * right for a *configured* value — a retention window is "30d", not
 * "29d 23h". This is for a value that is counting down, where exact
 * divisions essentially never happen: a session unlock with 43,187 seconds
 * left is "12h", not "43187s".
 *
 * Mirrors `identity_protect::humanish` in the CLI, so the same unlock reads
 * the same in both front ends.
 */
export function fmtRoughDuration(secs: number): string {
  const n = Math.floor(secs);
  if (!Number.isFinite(n) || n <= 0) return "—";
  if (n <= 90) return `${n}s`;
  if (n <= 5400) return `${Math.ceil(n / 60)}m`;
  return `${Math.ceil(n / 3600)}h`;
}

// One byte formatter for the whole app (UX-19): this one stopped at GB
// with two decimals, which is how "8886.72 GB" sat under "8.7 TB".
export { fmtBytes, fmtSizeMb } from "./bytes";
