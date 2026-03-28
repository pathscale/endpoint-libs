import { Container, getContainer } from "@cloudflare/containers";

export class WsEchoContainer extends Container {
  defaultPort = 8080;
  sleepAfter = "10m";
}

export interface Env {
  CONTAINER: DurableObjectNamespace<WsEchoContainer>;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    return getContainer(env.CONTAINER, "singleton").fetch(request);
  },
};
