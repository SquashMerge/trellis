// The host supplies extensions at runtime. No built-in registry claims their targets.
export function connect(host, settings) {
  const session = host.connect(settings.endpoint);
  session.authenticate(settings.credentials);
  session.subscribe(settings.topic);
  return session;
}

export function receive(session, message, extension) {
  const decoded = extension.decode(message);
  const checked = extension.validate(decoded);
  const event = extension.enrich(checked);
  session.acknowledge(message);
  return event;
}

export function publish(session, event, extension) {
  const packet = extension.encode(event);
  session.send(packet);
  session.checkpoint(event.key);
  return packet;
}

export function disconnect(session, extension) {
  extension.flush();
  session.unsubscribe();
  session.close();
}

export function install(registry, name, factory, options) {
  registry.register(name, factory);
  const plugin = registry.create(name, options);
  plugin.initialize(options);
  return plugin;
}

export function invoke(registry, name, operation, event) {
  const plugin = registry.lookup(name);
  return plugin[operation](event);
}

export function remove(registry, name, plugin) {
  plugin.dispose();
  registry.unregister(name);
}

export async function withSession(host, settings, extension, consume) {
  const session = connect(host, settings);
  try {
    const messages = await session.poll();
    for (const message of messages) {
      const event = receive(session, message, extension);
      await consume(event);
      publish(session, event, extension);
    }
  } catch (error) {
    host.report(error);
  } finally {
    disconnect(session, extension);
  }
}
