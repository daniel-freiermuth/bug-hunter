import { SvelteURLSearchParams } from "svelte/reactivity";

/** Split the hash route, optional finding id, and independently encoded filter parameters. */
export function parseHash(hash: string): { page: string; focusId: number | null; params: URLSearchParams } {
  const raw = hash.replace(/^#/, "");
  const question = raw.indexOf("?");
  const target = (question < 0 ? raw : raw.slice(0, question)) || "inbox";
  const params = new SvelteURLSearchParams(question < 0 ? "" : raw.slice(question + 1));
  const colon = target.indexOf(":");
  const id = colon < 0 ? NaN : Number(target.slice(colon + 1));
  return {
    page: colon < 0 ? target : target.slice(0, colon),
    focusId: Number.isSafeInteger(id) && id > 0 ? id : null,
    params,
  };
}

/** Hash routes remain the source of truth, including same-page Back/Forward. */
class Navigation {
  hash = $state(typeof window === "undefined" ? "" : window.location.hash);

  get route() {
    return parseHash(this.hash);
  }

  /**
   * Link to a finding on All Findings that keeps the current view's filters
   * and sort, so the finding opens among the cards it was found among. Every
   * page uses the same parameter names; pages without filters carry none.
   */
  findingHref(id: number): string {
    const query = this.route.params.toString();
    return query ? `#findings:${id}?${query}` : `#findings:${id}`;
  }

  /** Restore the current browser entry after hash navigation or Back/Forward. */
  sync(): void {
    this.hash = window.location.hash;
  }

  /** Open a page as a new history entry, clearing filters belonging to the old page. */
  navigate(target: string): void {
    this.commit(`#${target}`, false);
  }

  /**
   * Edit filter parameters without losing the page; replace coalesces slider
   * input. The focused finding is kept unless `keepsFocus` says the edited
   * parameters would hide it.
   */
  updateParams(
    update: (params: URLSearchParams) => void,
    replace = false,
    keepsFocus: (params: URLSearchParams) => boolean = () => true,
  ): void {
    const question = this.hash.indexOf("?");
    let target = (question < 0 ? this.hash : this.hash.slice(0, question)) || "#inbox";
    const params = this.route.params;
    update(params);
    if (!keepsFocus(params)) target = target.replace(/:.*$/, "");
    const query = params.toString();
    this.commit(query ? `${target}?${query}` : target, replace);
  }

  /** Update URL and reactive state together; pushState does not emit hashchange. */
  private commit(hash: string, replace: boolean): void {
    if (window.location.hash === hash) return;
    if (replace) window.history.replaceState(null, "", hash);
    else window.history.pushState(null, "", hash);
    this.hash = window.location.hash;
  }
}

export const navigation = new Navigation();
