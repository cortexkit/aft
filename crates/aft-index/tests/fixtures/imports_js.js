// Test fixture: JavaScript file with external and relative imports
// Used by integration tests for add_import command

import React from 'react';
import { render } from 'react-dom';
import express from 'express';

import { handler } from './routes';
import { db } from '../database';

export function startServer() {
  const app = express();
  app.get('/', handler);
  return app;
}
