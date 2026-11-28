declare const vi: {
  mock(
    path: string,
    factory: (importOriginal: <T>() => Promise<T>) => Promise<unknown>,
  ): void;
};
