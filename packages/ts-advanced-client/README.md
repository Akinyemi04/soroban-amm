# Advanced TypeScript Client with Middleware

This client provides a chainable middleware API for transaction signing, validation,
price feeds and plugins.

Usage example:

```ts
import { AdvancedClient, signerMiddleware, priceFeedMiddleware } from '@example/ts-advanced-client'

const client = new AdvancedClient()
client.middleware()
  .use(priceFeedMiddleware(async () => 123.45))
  .use(signerMiddleware({ sign: async (tx)=> ({...tx, signed:true}) }))

await client.sendTransaction({ kind: 'swap', amount: 1000 })
```
