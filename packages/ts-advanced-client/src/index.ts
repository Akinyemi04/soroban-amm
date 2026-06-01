export type MiddlewareContext = {
  tx: any
  metadata?: Record<string, unknown>
}

export type Middleware = (ctx: MiddlewareContext, next: () => Promise<void>) => Promise<void>

export class MiddlewareStack {
  private middlewares: Middleware[] = []

  use(m: Middleware) {
    this.middlewares.push(m)
    return this
  }

  async run(ctx: MiddlewareContext) {
    let idx = -1
    const dispatch = async (i: number): Promise<void> => {
      if (i <= idx) throw new Error('next() called multiple times')
      idx = i
      const fn = this.middlewares[i]
      if (fn) {
        await fn(ctx, () => dispatch(i + 1))
      }
    }
    await dispatch(0)
  }
}

export class AdvancedClient {
  private stack = new MiddlewareStack()

  middleware(): MiddlewareStack {
    return this.stack
  }

  async sendTransaction(tx: any) {
    const ctx: MiddlewareContext = { tx, metadata: {} }
    // run middlewares in order
    await this.stack.run(ctx)
    // after middleware stack, tx should be signed/validated
    // TODO: submit to network
    return { status: 'ok', tx }
  }
}

// Example plugin: signer middleware
export function signerMiddleware(signer: { sign: (tx: any) => Promise<any> }): Middleware {
  return async (ctx, next) => {
    ctx.tx = await signer.sign(ctx.tx)
    await next()
  }
}

// Example plugin: price feed middleware
export function priceFeedMiddleware(getPrice: () => Promise<number>): Middleware {
  return async (ctx, next) => {
    const price = await getPrice()
    ctx.metadata = { ...(ctx.metadata || {}), price }
    await next()
  }
}
