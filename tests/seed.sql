CREATE DATABASE ShopDB;
GO
USE ShopDB;
GO
CREATE SCHEMA Sales;
GO
CREATE TABLE Sales.Customers (
    CustomerID   INT IDENTITY(1,1) PRIMARY KEY,
    CustomerName NVARCHAR(100) NOT NULL,
    Email        NVARCHAR(200) NULL,
    CustomerSince DATE NOT NULL,
    CreditLimit  DECIMAL(12,2) NULL,
    IsActive     BIT NOT NULL DEFAULT 1
);
CREATE TABLE Sales.Orders (
    OrderID     INT IDENTITY(1,1) PRIMARY KEY,
    CustomerID  INT NOT NULL REFERENCES Sales.Customers(CustomerID),
    OrderDate   DATETIME2 NOT NULL,
    TotalAmount DECIMAL(12,2) NOT NULL,
    Status      VARCHAR(20) NOT NULL,
    Notes       NVARCHAR(MAX) NULL,
    Shipped     BIT NULL
);
CREATE TABLE dbo.AuditLog (
    LogID     BIGINT IDENTITY(1,1) PRIMARY KEY,
    Message   NVARCHAR(400) NOT NULL,
    LoggedAt  DATETIME2 NOT NULL,
    Severity  TINYINT NOT NULL,
    TraceGuid UNIQUEIDENTIFIER NULL,
    Payload   VARBINARY(64) NULL
);
GO
EXEC sys.sp_cdc_enable_db;
EXEC sys.sp_cdc_enable_table @source_schema = 'dbo', @source_name = 'AuditLog', @role_name = NULL;
GO
INSERT INTO Sales.Customers (CustomerName, Email, CustomerSince, CreditLimit, IsActive)
SELECT CONCAT('Customer ', n), CONCAT('cust', n, '@example.com'), DATEADD(day, -n, '2026-01-01'),
       n * 100.50, CASE WHEN n % 5 = 0 THEN 0 ELSE 1 END
FROM (SELECT TOP 500 ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) n FROM sys.all_objects) x;
INSERT INTO Sales.Orders (CustomerID, OrderDate, TotalAmount, Status, Notes, Shipped)
SELECT (n % 500) + 1, DATEADD(hour, -n, '2026-06-01'), n * 9.99,
       CASE n % 3 WHEN 0 THEN 'open' WHEN 1 THEN 'shipped' ELSE 'cancelled' END,
       CASE WHEN n % 7 = 0 THEN NULL ELSE CONCAT('note ', n) END,
       CASE WHEN n % 4 = 0 THEN NULL WHEN n % 2 = 0 THEN 1 ELSE 0 END
FROM (SELECT TOP 2000 ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) n FROM sys.all_objects) x;
INSERT INTO dbo.AuditLog (Message, LoggedAt, Severity, TraceGuid, Payload)
SELECT CONCAT('event ', n), DATEADD(minute, -n, '2026-06-01'), n % 5, NEWID(), CONVERT(VARBINARY(64), CONCAT('p', n))
FROM (SELECT TOP 300 ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) n FROM sys.all_objects) x;
GO
SELECT COUNT(*) AS Customers FROM Sales.Customers;
SELECT COUNT(*) AS Orders FROM Sales.Orders;
SELECT COUNT(*) AS AuditRows FROM dbo.AuditLog;
GO
