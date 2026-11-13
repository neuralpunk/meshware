<script lang="ts">
  // The demo, said out loud, on every screen (L1 P3).
  //
  // A demo mesh looks exactly like a real one — that is the point of it
  // being real — so the one thing it must never do is let somebody believe
  // they have set up the product. This band says what they are looking at
  // and offers the two things they can do about it: keep going for real, or
  // throw it away. Both end the demo; neither is hidden behind Settings.
  import { api, errText } from "../api";
  import { app } from "../state.svelte";

  const inDemo = $derived(app.boot?.demo === true);
  let busy = $state(false);

  async function stop(message: string) {
    busy = true;
    try {
      await api.demoStop();
      // Back to where the wizard left off: no mesh, five ways in — and none
      // of the demo's tables, polls or founder identity carried along to
      // whichever mesh comes next (DIR-5).
      await app.toWizard();
      app.flash(message);
    } catch (e) {
      app.flash(errText(e));
    } finally {
      busy = false;
    }
  }
</script>

{#if inDemo}
  <div
    class="flex flex-none flex-wrap items-center gap-x-3 gap-y-1.5 px-[22px] py-2.5 text-sm"
    style="background: var(--accent-wash); color: var(--accent-ink); border-bottom: 1px solid var(--edge);"
  >
    <span class="font-semibold">This is a demo.</span>
    <span>
      Three machines, all running on this computer. Everything you see is real — real machines
      in a real mesh, running real jobs — and none of it is kept.
    </span>
    <button
      class="ml-auto font-semibold disabled:opacity-40"
      disabled={busy}
      onclick={() => stop("demo cleared — set up a real mesh")}
    >
      Turn this into a real mesh →
    </button>
    <button
      class="font-semibold opacity-70 disabled:opacity-40"
      disabled={busy}
      onclick={() => stop("demo stopped and removed")}
    >
      Stop and remove
    </button>
  </div>
{/if}
