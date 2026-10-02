import type { StoreApi } from "zustand";
import type { WorkspaceRuntimeRegistry } from "@/features/workspace/runtime/workspace-runtime-registry";
import {
  buildWindowTitleContext,
  type WindowTitleBufferState,
  type WindowTitlePaneState,
  type WindowTitleProject,
} from "../utils/window-title-context";

interface WindowTitleSourceDependencies {
  registry: Pick<WorkspaceRuntimeRegistry, "getActiveWorkspaceId" | "getWorkspace" | "subscribe">;
  tabs: {
    getState: () => { projectTabs: readonly WindowTitleProject[] };
    subscribe: (listener: () => void) => () => void;
  };
  keyboardContext?: {
    getState: () => { contexts: { terminalFocus?: boolean } };
    subscribe: (listener: () => void) => () => void;
  };
}

const PANE_STORE_KEY = "pane";
const BUFFER_STORE_KEY = "editor-buffer";

export function createWindowTitleSource({ registry, tabs, keyboardContext }: WindowTitleSourceDependencies) {
  return {
    readContext: () => {
      const workspaceId = registry.getActiveWorkspaceId();
      const runtime = registry.getWorkspace(workspaceId);
      const panes = runtime?.stores.get(PANE_STORE_KEY) as StoreApi<WindowTitlePaneState> | undefined;
      const buffers = runtime?.stores.get(BUFFER_STORE_KEY) as StoreApi<WindowTitleBufferState> | undefined;
      const context = buildWindowTitleContext({
        projects: tabs.getState().projectTabs,
        workspaceId,
        status: runtime?.status,
        panes: panes?.getState(),
        buffers: buffers?.getState(),
      });
      // 底部终端使用独立标签状态，只读现有键盘焦点，不改变编辑器活动分屏。
      if (keyboardContext?.getState().contexts.terminalFocus) context.fileName = null;
      return context;
    },
    subscribe: (listener: () => void) => {
      let paneStore: StoreApi<unknown> | undefined;
      let bufferStore: StoreApi<unknown> | undefined;
      let unsubscribePane: (() => void) | undefined;
      let unsubscribeBuffer: (() => void) | undefined;

      const bindActiveStores = () => {
        const runtime = registry.getWorkspace(registry.getActiveWorkspaceId());
        const nextPaneStore = runtime?.stores.get(PANE_STORE_KEY);
        const nextBufferStore = runtime?.stores.get(BUFFER_STORE_KEY);
        if (nextPaneStore !== paneStore) {
          unsubscribePane?.();
          paneStore = nextPaneStore;
          unsubscribePane = paneStore?.subscribe(listener);
        }
        if (nextBufferStore !== bufferStore) {
          unsubscribeBuffer?.();
          bufferStore = nextBufferStore;
          unsubscribeBuffer = bufferStore?.subscribe(listener);
        }
      };

      // 仅观察已经创建的工作区状态，不为窗口标题初始化编辑器或项目运行时。
      bindActiveStores();
      const unsubscribeRegistry = registry.subscribe(() => {
        bindActiveStores();
        listener();
      });
      const unsubscribeTabs = tabs.subscribe(listener);
      let terminalFocus = keyboardContext?.getState().contexts.terminalFocus;
      const unsubscribeKeyboard = keyboardContext?.subscribe(() => {
        const nextTerminalFocus = keyboardContext.getState().contexts.terminalFocus;
        if (nextTerminalFocus === terminalFocus) return;
        terminalFocus = nextTerminalFocus;
        listener();
      });

      return () => {
        unsubscribeRegistry();
        unsubscribeTabs();
        unsubscribeKeyboard?.();
        unsubscribePane?.();
        unsubscribeBuffer?.();
      };
    },
  };
}
