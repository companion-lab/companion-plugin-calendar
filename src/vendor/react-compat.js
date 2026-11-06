// Host-react compat for the vendored react-big-calendar bundle (see
// scripts/build-vendor.mjs). The plugin build aliases bare "react" imports to
// the SDK shim, which does not export createRef — react-big-calendar needs
// it — so the vendor bundle is pre-built against this module instead.
//
// Like the SDK shim, React itself comes from the host app via
// globalThis.__companion_host__ — the same instance the host renders with, so
// hooks and elements stay interoperable. Plugins never bundle their own copy.

const host = globalThis.__companion_host__;

if (!host || !host.React) {
  throw new Error(
    "companion plugin runtime not present: this bundle must be activated by the companion host, not loaded directly",
  );
}

const React = host.React;

export default React;
export const {
  Children,
  Component,
  Fragment,
  PureComponent,
  StrictMode,
  Suspense,
  cloneElement,
  createContext,
  createElement,
  createRef,
  forwardRef,
  isValidElement,
  memo,
  useCallback,
  useContext,
  useDebugValue,
  useDeferredValue,
  useEffect,
  useId,
  useImperativeHandle,
  useInsertionEffect,
  useLayoutEffect,
  useMemo,
  useReducer,
  useRef,
  useState,
  useSyncExternalStore,
  useTransition,
} = React;
