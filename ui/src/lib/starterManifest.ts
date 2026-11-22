/**
 * Starter template for a new raw-manifest catalog application (`runtime: manifest`):
 * a Deployment + Service with `${name}` / `${name:=default}` substitution variables.
 */
export const DEFAULT_STARTER_MANIFEST = `apiVersion: apps/v1
kind: Deployment
metadata:
  name: \${appName}
  labels:
    app: \${appName}
spec:
  replicas: \${replicas:=2}
  selector:
    matchLabels:
      app: \${appName}
  template:
    metadata:
      labels:
        app: \${appName}
    spec:
      containers:
        - name: app
          image: nginx:alpine
          ports:
            - containerPort: 80
---
apiVersion: v1
kind: Service
metadata:
  name: \${appName}
spec:
  selector:
    app: \${appName}
  ports:
    - port: 80
      targetPort: 80`
