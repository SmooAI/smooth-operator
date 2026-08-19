/**
 * The widget's Rich Interaction card registry — `kind` → card component. An
 * `interaction_required` event looks the card up by `kind` and renders it in the
 * overlay slot above the composer. Registering a card here IS declaring the kind's
 * render capability; adding a kind is one card component + one entry.
 *
 * The registry value type is intentionally loose (`InteractionCardProps`, with a
 * kind-shaped `spec`/`values`) so a dynamic `interactionCards[kind]` lookup yields
 * a single component type rather than a union — each card stays fully typed against
 * its own `Spec`/`Values` internally.
 */
import type { ComponentType } from 'react';
import { ChoicesCard } from './ChoicesCard.js';
import { IdentityIntakeCard } from './IdentityIntakeCard.js';

/** The common shape every interaction card satisfies. `spec`/`values` are
 * kind-shaped and left loose here; the concrete card narrows them. */
export interface InteractionCardProps {
    spec: unknown;
    reason?: string;
    onSubmit: (values: any) => void;
    onDecline: () => void;
    errors?: { field: string; message: string }[];
    busy?: boolean;
    className?: string;
}

export const interactionCards: Record<string, ComponentType<InteractionCardProps>> = {
    choices: ChoicesCard as ComponentType<InteractionCardProps>,
    identity_intake: IdentityIntakeCard as ComponentType<InteractionCardProps>,
};
