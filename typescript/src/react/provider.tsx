/**
 * SmoothOperatorProvider — optional context that owns a single shared
 * {@link SmoothAgentClient} for a subtree.
 *
 * You don't need this to use the hooks (`useConversation` can take a `url`
 * directly), but a provider is handy when several components share one WS
 * connection, or when you want to construct the client yourself (custom
 * transport, auth-token refresh, etc.) and hand it down.
 */
import { SmoothAgentClient } from '../client.js';
import { createContext, createElement, useContext, useMemo, type ReactNode } from 'react';

export interface SmoothOperatorContextValue {
    client: SmoothAgentClient | null;
    /** The base WS URL, if the provider was given one (so hooks can construct per-session clients). */
    url: string | null;
}

const SmoothOperatorContext = createContext<SmoothOperatorContextValue | null>(null);

export interface SmoothOperatorProviderProps {
    /** A pre-constructed client to share. Takes precedence over `url`. */
    client?: SmoothAgentClient;
    /** A WS URL the provider memoizes a client from, if no `client` is passed. */
    url?: string;
    children: ReactNode;
}

export function SmoothOperatorProvider({ client, url, children }: SmoothOperatorProviderProps) {
    const value = useMemo<SmoothOperatorContextValue>(() => {
        if (client) return { client, url: url ?? null };
        return { client: null, url: url ?? null };
    }, [client, url]);
    return createElement(SmoothOperatorContext.Provider, { value }, children);
}

/** Read the nearest provider value, or `null` if there is no provider. */
export function useSmoothOperator(): SmoothOperatorContextValue | null {
    return useContext(SmoothOperatorContext);
}
