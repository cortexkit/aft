// Test fixture: TypeScript file with multiple import groups
// Used by integration tests for add_import command

import React from 'react';
import { useState, useEffect } from 'react';
import { z } from 'zod';
import type { FC } from 'react';

import { helper } from './utils';
import { Config } from '../config';
import type { AppState } from './types';

export function App(): string {
  const [count, setCount] = useState(0);

  useEffect(() => {
    console.log('mounted');
  }, []);

  return String(count);
}

export const config: Config = {
  name: 'test',
};
