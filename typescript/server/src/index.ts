/**
 * @smooai/smooth-operator-server — a native TypeScript server for the
 * smooth-operator wire protocol.
 *
 * A WebSocket service that speaks the same protocol as the Rust
 * (`rust/smooth-operator-server`) and C# (`dotnet/server`) servers, and runs the
 * published `@smooai/smooth-operator-core` engine in-process per turn. The TS
 * client (`../src`) is untouched; this is the SERVER half.
 *
 * Boot the local flavor in a few lines:
 *
 * ```ts
 * import { serveLocal } from '@smooai/smooth-operator-server';
 * import { MockLlmProvider } from '@smooai/smooth-operator-core';
 *
 * const server = await serveLocal({ chatClient: new MockLlmProvider().pushText('hi') });
 * console.log(`smooth-operator on ${server.url}`);
 * // ... use it ...
 * await server.close(); // graceful drain + stop
 * ```
 */
export { buildServer, serve, serveLocal } from './server.js';
export type { RunningServer, ServerOptions } from './server.js';

export { FrameDispatcher } from './frameDispatcher.js';
export type { AccessKnowledge, FrameDispatcherOptions } from './frameDispatcher.js';

export { ConfirmationRegistry } from './confirmation.js';

export { InteractionParkRegistry, InteractionRegistry, INTERACTION_TIMEOUT_MS, requestInteractionTool, submitInteractionTool, SUBMIT_INTERACTION_TOOL } from './interaction.js';
export type { InteractionAttach, InteractionFieldError, InteractionKind, InteractionOutcome, InteractionRequest, InteractionValidation, PendingInteraction, RaisedSpecs } from './interaction.js';
export { ChoicesKind, HEADER_MAX_CHARS, parseQuestions, validateChoices } from './choices.js';
export type { ChoiceAnswer, ChoiceFieldError, ChoiceOption, ChoiceQuestion, ChoiceValues } from './choices.js';
export { IdentityIntakeKind, normalizeEmail, normalizePhoneE164, parseFields, validateIntake } from './identityIntake.js';
export type { IntakeField, IntakeFieldError, IntakeFieldKey, IntakeValues } from './identityIntake.js';

export { DEFAULT_MAX_ITERATIONS, DEFAULT_MAX_TOKENS, DEFAULT_MODEL, DEFAULT_SYSTEM_PROMPT, TurnRunner } from './turnRunner.js';
export type { Sink, TurnResult, TurnRunnerOptions } from './turnRunner.js';

export { StaticMemoryProvider, type MemoryProvider } from './memory.js';
export { DirSkillResolver, isValidSkillName, resolveSection, skillSection, stripFrontmatter, SKILLS_DIR_ENV, type SkillResolver } from './skills.js';
export { assembleSystemPrompt, parseAgentConfig, StaticAgentConfigResolver } from './agentConfig.js';
export type { AgentConfig, AgentConfigResolver, EnabledTool } from './agentConfig.js';

export { gateTools } from './toolGating.js';
export type { ServerTool, SessionAuthenticator } from './toolGating.js';

export { availableChannels, isContactEmpty } from './otp.js';
export type { OtpChannel, OtpContact, OtpDelivery, OtpError, OtpRefusal, OtpService, OtpVerifyOutcome } from './otp.js';

export {
    advanceStep,
    DEFAULT_JUDGE_MODEL,
    judgeStep,
    nextStep,
    parseWorkflow,
    renderWorkflowPromptSection,
    resolveCurrentStep,
} from './workflow.js';
export type { ConversationWorkflow, ConversationWorkflowStep, JudgeStepInput, WorkflowJudgeVerdict } from './workflow.js';

export { createGatewayModelCeilingResolver, extractModelCeilings } from './modelCeiling.js';
export type { FetchLike, ModelCeilingResolver } from './modelCeiling.js';

export { InMemorySessionStore } from './sessionStore.js';
export type { MessageDirection, SessionStore, StoredMessage, StoredSession } from './sessionStore.js';

export { ANONYMOUS_ACCESS, ANONYMOUS_PRINCIPAL, LocalTokenVerifier, NoAuthVerifier, TrustedTokenVerifier } from './auth.js';
export type { AccessContext, AuthVerifier, Principal } from './auth.js';

export { InMemoryBackplane } from './backplane.js';
export type { Backplane, BackplaneSink } from './backplane.js';

export { aclForGroups, DeterministicEmbedder, PostgresKnowledgeStore, PUBLIC_ACL } from './postgresKnowledge.js';
export type { DocumentAcl, Embedder, IngestView, KnowledgeDocument, KnowledgeQueryResult, KnowledgeView } from './postgresKnowledge.js';

export * as protocol from './protocol.js';
export type { Citation, Frame } from './protocol.js';

export { parseFiles, parseImages, withUserImages } from './toolContext.js';
export type { ImageDetail, ToolContext, ToolProvider, UserFile, UserImage } from './toolContext.js';
