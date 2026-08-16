/* eslint-disable @typescript-eslint/no-explicit-any -- SST config uses ambient $-globals */
/// <reference path="./.sst/platform/config.d.ts" />

/**
 * SST v4 app — the AWS-serverless deploy path for the smooth-operator
 * **management console** (Phase 12, increment 2).
 *
 * The console is a Next.js 15 App Router app deployed on `sst.aws.Nextjs`
 * (OpenNext → Lambda + CloudFront). It is a pure read client of the
 * `smooth-operator` admin API (`/admin/*`); the only deploy-time wiring it needs
 * is:
 *
 *   - `ADMIN_API_URL` — the base URL of the running `smooth-operator-server`
 *     (the axum service that mounts `/admin/*` alongside `/ws`).
 *   - The **OpenAuth issuer** — for production (`CONSOLE_AUTH` unset ⇒ openauth),
 *     an `sst.aws.Auth` component issues the BYO JWTs the console exchanges. We
 *     reference its issuer URL + a client id here. The Smoo-identity (hosted)
 *     alternative points `OPENAUTH_ISSUER` at `auth.smoo.ai` instead and skips
 *     the local `sst.aws.Auth`.
 *
 * NEVER deploy locally — CI owns deploys. Verification here is `npx tsc --noEmit`
 * + `sst` synth, not `sst deploy`.
 *
 * A future `Console` construct could live in `@smooai/deploy` (alongside the
 * existing `SmoothAgentApi`) to make this wiring reusable across stages; for now
 * it is declared inline.
 */

export default $config({
    app(input) {
        return {
            name: 'smooth-operator-console',
            removal: input?.stage === 'production' ? 'retain' : 'remove',
            protect: ['production'].includes(input?.stage ?? ''),
            home: 'aws',
            providers: {
                aws: {
                    region: (process.env.AWS_REGION as any) ?? 'us-east-1',
                },
            },
        };
    },

    async run() {
        // --- BYO auth issuer (SST OpenAuth) ----------------------------------
        // `sst.aws.Auth` runs the OpenAuth issuer (authorize/token endpoints) the
        // console drives. For the Smoo-identity (hosted) path, drop this and set
        // OPENAUTH_ISSUER = 'https://auth.smoo.ai' on the site env instead.
        const auth = new (sst as any).aws.Auth('ConsoleAuth', {
            // The issuer authorizer function lives in this repo's deploy package;
            // see deploy/sst/README.md. Placeholder handler path until the
            // OpenAuth issuer Lambda is wired in increment 3.
            authorizer: {
                handler: 'auth/issuer.handler',
            },
        });

        // --- The admin API base URL ------------------------------------------
        // In a full deploy this references the SmoothAgentApi output (the axum
        // service URL). Wired via env/secret so the console resolves it at
        // runtime; defaults to the local dev server for `sst dev`.
        const adminApiUrl = new (sst as any).Secret('AdminApiUrl', 'http://127.0.0.1:8840');

        // --- The Next.js console site ----------------------------------------
        new (sst as any).aws.Nextjs('Console', {
            path: '.',
            environment: {
                CONSOLE_AUTH: 'openauth',
                ADMIN_API_URL: (adminApiUrl as any).value,
                OPENAUTH_ISSUER: (auth as any).url,
                OPENAUTH_CLIENT_ID: 'smooth-console',
                BACKEND_AUTH_MODE: 'jwt',
            },
            link: [auth, adminApiUrl],
        });
    },
});
